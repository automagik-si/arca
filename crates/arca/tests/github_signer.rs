//! The GitHub signer against a fake GitHub (`tests/github/fake.rs`): the device-flow connection end to end, with the
//! grant used once and deleted, the runners registered with the person's labels and their own keys, every step shown and
//! logged, and no credential in anything the signer shows or writes outside its private store.

#[path = "github/fake.rs"]
mod fake;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca::github_api::Secret;
use arca::github_oauth::{FlowState, OauthFlow, OauthRequest};
use arca::github_signer::{Mode, Signer, Status};
use arca::handlers::Clock;
use arca_mcp::Caller;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use arca::github_app::{JobRef, MintRefused, MintRequest, RUNNER_GROUP_NAME};
use fake::{
    APP_CLIENT_ID, APP_SLUG, CLIENT_ID, CLIENT_SECRET, Fake, GRANT, INSTALLATION, JIT,
    MANIFEST_CODE, OWNER_ID, SECRETS, USER_CODE,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "arca-github-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("a scratch directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis() as u64
    })
}

/// A signer on `fake`, with its OAuth App set and small runner keys.
fn signer(scratch: &Scratch, fake: &Fake) -> Signer {
    let signer = Signer::open(&scratch.0, fake.endpoints(), clock())
        .expect("the signer opens")
        .with_runner_key_bits(1024);
    signer
        .set_oauth_app(CLIENT_ID, &Secret::new(CLIENT_SECRET))
        .expect("the OAuth App is set");
    signer
}

fn request(org: &str, group: Option<&str>, runners: u32) -> OauthRequest {
    OauthRequest {
        account: org.to_string(),
        group: group.map(str::to_string),
        labels: vec!["linux".to_string(), "big-box".to_string()],
        runners,
        name_prefix: Some("acme-box".to_string()),
        caller: "token-ops".to_string(),
    }
}

/// Waits, at most twenty seconds, until the flow leaves `waitingForPerson` and `registering`.
async fn settled(signer: &Signer, id: &str) -> OauthFlow {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let flow = signer.oauth_flow(id).expect("the flow is listed");
        if matches!(flow.state, FlowState::Done | FlowState::Failed) {
            return flow;
        }
        assert!(
            Instant::now() < deadline,
            "the flow did not settle: {flow:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every file under `dir` but the runners' own credentials, as text.
fn shown_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                // The private store's credential folders: runners' files, App keys and pushed configurations.
                if !matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("runners" | "keys" | "intake")
                ) {
                    stack.push(path);
                }
            } else if path.file_name().and_then(|n| n.to_str()) != Some("oauth-app.secret") {
                out.push((path.clone(), fs::read_to_string(&path).unwrap_or_default()));
            }
        }
    }
    out
}

fn assert_no_secret(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{what} holds a credential");
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_flow_registers_the_runners_with_the_persons_labels_and_deletes_the_grant() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);

    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 2))
        .await
        .expect("the flow starts");
    assert_eq!(flow.user_code, USER_CODE);
    assert_eq!(flow.verification_uri, "https://github.com/login/device");
    assert_eq!(flow.state, FlowState::WaitingForPerson);
    assert!(flow.steps[0].text.contains(USER_CODE));
    // Nothing is asked of the organisation while the person has not authorised.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        signer.oauth_flow(&flow.id).unwrap().state,
        FlowState::WaitingForPerson
    );
    assert!(fake.with(|s| s.runners.is_empty()));

    fake.with(|s| s.authorized = true);
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    assert!(done.steps.iter().all(|step| step.ok), "{:#?}", done.steps);

    // The runners: in the named group, with exactly the person's labels, persistent, each with its own key.
    let runners = fake.with(|s| s.runners.clone());
    assert_eq!(runners.len(), 2);
    let names: Vec<&str> = runners
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme-box-1", "acme-box-2"]);
    for runner in &runners {
        assert_eq!(runner["group"], 7);
        assert_eq!(runner["ephemeral"], false);
        assert_eq!(
            runner["labels"],
            json!([
                { "id": 0, "name": "linux", "type": "user" },
                { "id": 0, "name": "big-box", "type": "user" },
            ])
        );
    }
    assert_ne!(runners[0]["publicKey"], runners[1]["publicKey"]);

    // The grant: used for the registration token and the group list only, then deleted with the App's secret.
    let deletes = fake.seen("DELETE", &format!("/api/applications/{CLIENT_ID}/grant"));
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].body, json!({ "access_token": GRANT }));
    assert!(fake.with(|s| s.grant_deleted));
    let grant_uses: Vec<String> = fake.with(|s| {
        s.seen
            .iter()
            .filter(|seen| seen.auth == format!("bearer:{GRANT}"))
            .map(|seen| format!("{} {}", seen.method, seen.path))
            .collect()
    });
    assert_eq!(
        grant_uses,
        [
            "POST /api/orgs/acme/actions/runners/registration-token",
            "GET /api/orgs/acme/actions/runner-groups",
        ]
    );
    let last = fake.with(|s| s.seen.last().cloned().expect("a request"));
    assert_eq!(
        last.method, "DELETE",
        "the grant's deletion is the last request"
    );

    // The binding: active, no secret, its runners listed.
    let binding = signer.bindings().pop().expect("a binding");
    assert_eq!(binding.mode, Mode::Oauth);
    assert_eq!(binding.status, Status::Active);
    assert_eq!(binding.account, "acme");
    assert_eq!(binding.runner_group.as_ref().unwrap().id, 7);
    assert_eq!(
        binding.label_sets,
        vec![vec!["linux".to_string(), "big-box".to_string()]]
    );
    assert_eq!(binding.runners.len(), 2);
    assert_eq!(done.binding.as_deref(), Some(binding.id.as_str()));
    let info = signer.binding(&binding.id).expect("the launcher's view");
    assert_eq!(info.org_url, format!("{}/web/acme", fake.base));
    assert_eq!(info.runner_group_id, Some(7));

    // Each runner's files: what the runner reads, 0600, its key the one GitHub was given.
    let files = signer
        .runner_files(&binding.id, "acme-box-1")
        .expect("the runner's files");
    let settings: Value = serde_json::from_str(&files.runner).unwrap();
    assert_eq!(settings["agentId"], runners[0]["id"]);
    assert_eq!(settings["agentName"], "acme-box-1");
    assert_eq!(settings["poolId"], 7);
    assert_eq!(settings["useV2Flow"], true);
    assert_eq!(settings["gitHubUrl"], format!("{}/web/acme", fake.base));
    let credentials: Value = serde_json::from_str(files.credentials.expose()).unwrap();
    assert_eq!(credentials["scheme"], "OAuth");
    assert_eq!(
        credentials["data"]["authorizationUrl"],
        format!("{}/token/{}", fake.base, runners[0]["id"])
    );
    let params: Value = serde_json::from_str(files.rsaparams.expose()).unwrap();
    let modulus = params["modulus"].as_str().unwrap();
    assert!(runners[0]["publicKey"].as_str().unwrap().contains(modulus));
    let size = STANDARD.decode(modulus).unwrap().len();
    assert_eq!(
        STANDARD
            .decode(params["d"].as_str().unwrap())
            .unwrap()
            .len(),
        size
    );
    for half in ["p", "q", "dp", "dq", "inverseQ"] {
        assert_eq!(
            STANDARD
                .decode(params[half].as_str().unwrap())
                .unwrap()
                .len(),
            size / 2,
            "{half}"
        );
    }
    let folder = scratch
        .0
        .join("github/runners")
        .join(&binding.id)
        .join("acme-box-1");
    for name in [".runner", ".credentials", ".credentials_rsaparams"] {
        assert_eq!(mode_of(&folder.join(name)), 0o600, "{name}");
    }
    assert_eq!(mode_of(&scratch.0.join("github")), 0o700);
    assert!(signer.runner_files(&binding.id, "acme-box-9").is_err());

    // The audit log: the start, the grant, each registration with who and what, the grant's deletion, the connection.
    let audit = signer.audit_log(None, 100);
    let kinds: Vec<&str> = audit.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "oauthAppSet",
            "oauthStart",
            "oauthGranted",
            "register",
            "register",
            "grantDeleted",
            "connect"
        ]
    );
    let register = &audit[3];
    assert_eq!(register["caller"], "token-ops");
    assert_eq!(register["account"], "acme");
    assert_eq!(register["runnerGroupId"], 7);
    assert_eq!(register["runnerName"], "acme-box-1");
    assert_eq!(register["labels"], json!(["linux", "big-box"]));
    assert_eq!(register["outcome"], "registered");
    assert!(register["atText"].as_str().unwrap().ends_with('Z'));
    assert_eq!(signer.audit_log(Some(&binding.id), 100).len(), 3);

    // No credential in what the signer shows or keeps outside the runners' own folders and the App's secret file.
    assert_no_secret("the flow", &serde_json::to_string(&done).unwrap());
    assert_no_secret(
        "the bindings",
        &serde_json::to_string(&signer.bindings()).unwrap(),
    );
    for (path, text) in shown_files(&scratch.0.join("github")) {
        assert_no_secret(&path.display().to_string(), &text);
    }
    assert_eq!(mode_of(&scratch.0.join("github/oauth-app.secret")), 0o600);

    // A signer opened again on the store has the binding.
    let again = Signer::open(&scratch.0, fake.endpoints(), clock()).unwrap();
    assert_eq!(again.bindings(), signer.bindings());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_group_named_the_default_group_is_used_and_said_and_a_missing_group_is_made() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.authorized = true);
    let signer = signer(&scratch, &fake);

    let flow = signer.oauth_start(request("acme", None, 1)).await.unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    let group = signer.bindings()[0].runner_group.clone().unwrap();
    assert_eq!(group.id, 1);
    assert!(group.warning.unwrap().contains("default runner group"));

    let flow = signer
        .oauth_start(OauthRequest {
            name_prefix: Some("second".to_string()),
            ..request("acme", Some("new-group"), 1)
        })
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    let made = fake.seen("POST", "/api/orgs/acme/actions/runner-groups");
    assert_eq!(made.len(), 1);
    assert_eq!(
        made[0].body,
        json!({ "name": "new-group", "visibility": "all", "allows_public_repositories": false })
    );
    assert!(
        done.steps
            .iter()
            .any(|step| step.text.contains("Arca made the runner group"))
    );
    assert_eq!(
        fake.with(|s| s.runners.last().unwrap()["group"].clone()),
        20
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_legacy_service_registers_agents_in_the_groups_pool() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.authorized = true;
        s.admin_flow = false;
    });
    let signer = signer(&scratch, &fake);
    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 1))
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    let runner = fake.with(|s| s.runners[0].clone());
    assert_eq!(runner["pool"], 1007);
    assert!(runner["publicKey"]["modulus"].is_string());
    let binding = signer.bindings().pop().unwrap();
    let files = signer.runner_files(&binding.id, "acme-box-1").unwrap();
    let settings: Value = serde_json::from_str(&files.runner).unwrap();
    assert_eq!(settings["poolId"], 1007);
    assert_eq!(settings["serverUrl"], format!("{}/tenant", fake.base));
    assert!(settings.get("useV2Flow").is_none());
    assert!(fake.with(|s| s.grant_deleted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_organisation_that_restricts_oauth_apps_is_told_to_grant_and_the_grant_is_still_deleted()
{
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.authorized = true;
        s.restricted = true;
    });
    let signer = signer(&scratch, &fake);
    let flow = signer.oauth_start(request("acme", None, 1)).await.unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Failed);
    assert!(
        done.steps
            .iter()
            .any(|step| !step.ok && step.text.contains("Grant button"))
    );
    assert!(
        fake.with(|s| s.grant_deleted),
        "a grant that arrived is always deleted"
    );
    assert!(signer.bindings().is_empty());
    assert!(fake.with(|s| s.runners.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_sign_in_registers_nothing_and_has_no_grant_to_delete() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.declined = true);
    let signer = signer(&scratch, &fake);
    let flow = signer.oauth_start(request("acme", None, 1)).await.unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Failed);
    assert!(done.steps.last().unwrap().text.contains("You declined"));
    assert!(
        fake.seen("DELETE", &format!("/api/applications/{CLIENT_ID}/grant"))
            .is_empty()
    );
    assert!(signer.bindings().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_runner_keeps_the_ones_before_it_and_says_so() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.authorized = true;
        s.refuse_name = Some("acme-box-2".to_string());
    });
    let signer = signer(&scratch, &fake);
    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 3))
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Failed);
    assert!(
        done.steps
            .iter()
            .any(|s| !s.ok && s.text.contains("register runner acme-box-2"))
    );
    let binding = signer
        .bindings()
        .pop()
        .expect("the runner that was registered is kept");
    assert_eq!(binding.runners.len(), 1);
    assert!(binding.why.unwrap().contains("1 of 3"));
    assert!(fake.with(|s| s.grant_deleted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_oauth_app_nothing_starts_and_bad_arguments_are_named() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = Signer::open(&scratch.0, fake.endpoints(), clock()).unwrap();
    let error = signer
        .oauth_start(request("acme", None, 1))
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("NoOauthApp"));
    let signer = self::signer(&scratch, &fake);
    for bad in [
        OauthRequest {
            runners: 0,
            ..request("acme", None, 1)
        },
        OauthRequest {
            account: "-acme".to_string(),
            ..request("acme", None, 1)
        },
        OauthRequest {
            labels: vec!["has space".to_string()],
            ..request("acme", None, 1)
        },
    ] {
        assert!(signer.oauth_start(bad).await.is_err());
    }
    assert!(fake.seen("POST", "/web/login/device/code").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_verbs_start_a_connection_and_show_its_steps_and_the_log() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = Signer::open(&scratch.0, fake.endpoints(), clock())
        .unwrap()
        .with_runner_key_bits(1024);
    let handlers: std::collections::HashMap<&str, Arc<dyn arca_mcp::ToolHandler>> =
        arca::github_handlers::handlers(&signer)
            .into_iter()
            .collect();
    let caller = Caller {
        token_id: "token-ops".to_string(),
        scope: arca_api::verbs::Scope::Operator,
        tools: None,
        limits: Default::default(),
    };
    let call = |verb: &'static str, args: Value| {
        let handler = Arc::clone(&handlers[verb]);
        let caller = caller.clone();
        tokio::task::spawn_blocking(move || handler.call(args, &caller))
    };

    let set = call(
        "github_oauth_app_set",
        json!({ "clientId": CLIENT_ID, "clientSecret": CLIENT_SECRET }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(set, json!({ "ok": true, "clientId": CLIENT_ID }));

    let started = call(
        "github_connect",
        json!({ "org": "acme", "group": "ci-pool", "labels": ["linux"], "runners": 1 }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(started["userCode"], USER_CODE);
    assert_eq!(started["state"], "waitingForPerson");
    let id = started["id"].as_str().unwrap().to_string();

    fake.with(|s| s.authorized = true);
    let done = settled(&signer, &id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);

    let status = call("github_connect_status", json!({ "id": id }))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status["state"], "done");
    assert!(status["steps"].as_array().unwrap().len() >= 5);
    let listed = call("github_bindings", json!({})).await.unwrap().unwrap();
    assert_eq!(listed["bindings"][0]["mode"], "oauth");
    let runner_name = listed["bindings"][0]["runners"][0]["name"]
        .as_str()
        .unwrap();
    assert!(runner_name.len() > 5, "{runner_name}");
    let log = call("github_audit", json!({ "limit": 2 }))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(log["lines"].as_array().unwrap().len(), 2);
    for value in [&status, &listed, &log] {
        assert_no_secret("a verb's answer", &value.to_string());
    }
    let unknown = call("github_connect_status", json!({ "id": "gho-none" }))
        .await
        .unwrap();
    assert!(unknown.is_err());
}

// ---- The App made from a manifest, and the mint ---------------------------------------------------------------------

fn labels(list: &[&str]) -> Vec<String> {
    list.iter().map(|label| label.to_string()).collect()
}

fn mint_request(binding: &str, name: &str, list: &[&str], scale_set: Option<u64>) -> MintRequest {
    MintRequest {
        binding: binding.to_string(),
        runner_name: name.to_string(),
        labels: labels(list),
        work_folder: None,
        scale_set_id: scale_set,
        job: None,
        caller: "pool:test".to_string(),
    }
}

/// Runs the manifest flow on `fake` up to an installed App, and answers the binding's id.
async fn installed_app(signer: &Signer, fake: &Fake) -> String {
    let start = signer
        .app_begin(
            "acme",
            &[labels(&["linux", "x64"]), labels(&["gpu"])],
            "https://arca.example.com",
            "page:passkey-1",
        )
        .expect("the flow begins");
    signer
        .app_created(MANIFEST_CODE, &start.state)
        .await
        .expect("GitHub hands the App over");
    fake.with(|s| s.installed = true);
    signer
        .app_installed(INSTALLATION, Some(&start.state))
        .await
        .expect("the installation is recorded")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_flow_makes_an_app_with_one_permission_and_mints_runners_for_its_label_sets() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);

    let start = signer
        .app_begin(
            "acme",
            &[labels(&["linux", "x64"]), labels(&["gpu"])],
            "https://arca.example.com",
            "page:passkey-1",
        )
        .expect("the flow begins");
    assert_eq!(
        start.action_url,
        format!(
            "{}/web/organizations/acme/settings/apps/new?state={}",
            fake.base, start.state
        )
    );
    let manifest: Value = serde_json::from_str(&start.manifest).unwrap();
    assert_eq!(
        manifest["default_permissions"],
        json!({ "organization_self_hosted_runners": "write" })
    );
    assert_eq!(manifest["default_events"], json!([]));
    assert_eq!(manifest["hook_attributes"]["active"], false);
    assert_eq!(manifest["public"], false);
    assert_eq!(
        manifest["redirect_url"],
        "https://arca.example.com/approve/github/created"
    );
    assert_eq!(
        manifest["setup_url"],
        "https://arca.example.com/approve/github/installed"
    );

    // A state Arca never gave is refused before GitHub is asked anything.
    assert!(
        signer
            .app_created(MANIFEST_CODE, "not-a-state")
            .await
            .is_err()
    );
    assert!(
        fake.seen(
            "POST",
            &format!("/api/app-manifests/{MANIFEST_CODE}/conversions")
        )
        .is_empty()
    );

    let install = signer
        .app_created(MANIFEST_CODE, &start.state)
        .await
        .expect("the App is handed over");
    assert_eq!(
        install,
        format!(
            "{}/web/apps/{APP_SLUG}/installations/new/permissions?target_id={OWNER_ID}&state={}",
            fake.base, start.state
        )
    );
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.mode, Mode::App);
    assert_eq!(binding.status, Status::AwaitingInstall);
    assert_eq!(binding.app.as_ref().unwrap().client_id, APP_CLIENT_ID);
    let key = scratch
        .0
        .join("github/keys")
        .join(format!("{}.pem", binding.id));
    assert_eq!(mode_of(&key), 0o600);
    assert_eq!(
        signer
            .mint(mint_request(&binding.id, "r-0", &["linux", "x64"], None))
            .await
            .unwrap_err(),
        MintRefused::NotInstalled
    );

    fake.with(|s| s.installed = true);
    let id = signer
        .app_installed(INSTALLATION, Some(&start.state))
        .await
        .expect("the installation is recorded");
    assert_eq!(id, binding.id);
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.status, Status::Active);
    let installation = binding.installation.clone().unwrap();
    assert_eq!(installation.id, INSTALLATION);
    assert_eq!(installation.repositories, vec!["acme/web", "acme/api"]);
    // Arca's own group, open to exactly the repositories the person picked, public repositories off.
    let made = fake.seen("POST", "/api/orgs/acme/actions/runner-groups");
    assert_eq!(
        made[0].body,
        json!({
            "name": RUNNER_GROUP_NAME,
            "visibility": "selected",
            "selected_repository_ids": [11, 12],
            "allows_public_repositories": false,
        })
    );
    let group = binding.runner_group.clone().unwrap();
    assert_eq!(
        (group.id, group.created_by_arca, group.warning.is_none()),
        (20, true, true)
    );
    let info = signer.binding(&binding.id).unwrap();
    assert_eq!(info.runner_group_id, Some(20));
    assert_eq!(info.org_url, format!("{}/web/acme", fake.base));

    // A runner per job, with exactly one of the binding's label sets, in any order.
    let minted = signer
        .mint(MintRequest {
            job: Some(JobRef {
                repo: "acme/web".to_string(),
                run_id: Some(9),
                ..JobRef::default()
            }),
            ..mint_request(&binding.id, "acme-job-1", &["x64", "linux"], None)
        })
        .await
        .expect("a runner is minted");
    assert!(minted.encoded_jit_config.expose().starts_with(JIT));
    assert_eq!(minted.labels, labels(&["linux", "x64"]));
    assert_eq!(minted.runner_group_id, Some(20));
    let runner = fake.with(|s| s.runners.last().unwrap().clone());
    assert_eq!(runner["group"], 20);
    assert_eq!(runner["labels"], json!(["linux", "x64"]));
    assert_eq!(runner["workFolder"], "_work");
    signer.attach_job(
        minted.mint_id,
        &JobRef {
            repo: "acme/web".to_string(),
            job_id: Some(77),
            ..JobRef::default()
        },
    );
    signer
        .mint(mint_request(&binding.id, "acme-job-2", &["gpu"], None))
        .await
        .expect("a second runner");
    // One installation token serves both mints.
    assert_eq!(
        fake.seen(
            "POST",
            &format!("/api/app/installations/{INSTALLATION}/access_tokens")
        )
        .len(),
        1
    );

    match signer
        .mint(mint_request(
            &binding.id,
            "acme-job-3",
            &["gpu", "linux"],
            None,
        ))
        .await
    {
        Err(MintRefused::LabelsNotAllowed { allowed }) => assert_eq!(allowed.len(), 2),
        other => panic!("labels outside the sets are refused: {other:?}"),
    }

    // A scale set's runner: the Actions service connection is made once and kept, and only a scale set of the
    // binding's own runner group gets one.
    fake.with(|s| s.scale_sets = vec![(3, 20), (4, 7)]);
    let connection = signer
        .actions_connection(&binding.id)
        .await
        .expect("a connection");
    assert_eq!(connection.service_url, format!("{}/tenant", fake.base));
    for name in ["scale-1", "scale-2"] {
        let minted = signer
            .mint(mint_request(&binding.id, name, &["linux", "x64"], Some(3)))
            .await
            .expect("a scale set's runner is minted");
        assert!(minted.encoded_jit_config.expose().starts_with(JIT));
    }
    assert_eq!(
        fake.seen("POST", "/api/actions/runner-registration").len(),
        1
    );
    assert_eq!(
        fake.with(|s| s.runners.last().unwrap()["scaleSet"].clone()),
        3
    );
    let runners = fake.with(|s| s.runners.len());
    assert_eq!(
        signer
            .mint(mint_request(
                &binding.id,
                "other-group",
                &["linux", "x64"],
                Some(4)
            ))
            .await
            .unwrap_err(),
        MintRefused::ScaleSetOutsideGroup {
            scale_set: 4,
            scale_set_group: Some(7),
            group: Some(20)
        }
    );
    assert_eq!(
        fake.with(|s| s.runners.len()),
        runners,
        "no runner is made for another group's scale set"
    );
    assert!(
        fake.seen(
            "POST",
            "/tenant/_apis/runtime/runnerscalesets/4/generatejitconfig"
        )
        .is_empty()
    );

    let audit = signer.audit_log(None, 100);
    let kinds: Vec<&str> = audit.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    for kind in [
        "appStart",
        "appCreated",
        "appInstalled",
        "sync",
        "mint",
        "job",
        "actionsConnection",
    ] {
        assert!(kinds.contains(&kind), "{kind} is logged: {kinds:?}");
    }
    let mints: Vec<&Value> = audit.iter().filter(|l| l["kind"] == "mint").collect();
    assert_eq!(mints.len(), 7, "every mint is logged, refused ones too");
    assert_eq!(mints[0]["outcome"], "refused");
    assert_eq!(mints[0]["refusal"], "NotInstalled");
    assert_eq!(mints[1]["outcome"], "minted");
    assert_eq!(mints[1]["caller"], "pool:test");
    assert_eq!(mints[1]["job"]["repo"], "acme/web");
    assert_eq!(mints[1]["labels"], json!(["x64", "linux"]));
    assert_eq!(mints[3]["refusal"], "LabelsNotAllowed");
    assert_eq!(mints[4]["runnerGroupId"], 20);
    assert_eq!(mints[4]["scaleSetId"], 3);
    assert_eq!(mints[6]["refusal"], "ScaleSetOutsideGroup");
    assert_eq!(mints[6]["scaleSetRunnerGroupId"], 7);
    for (path, text) in shown_files(&scratch.0) {
        assert_no_secret(&path.display().to_string(), &text);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_suspended_uninstalled_or_deleted_on_github_is_seen_at_the_next_mint() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    let mint = |name: &'static str| signer.mint(mint_request(&id, name, &["gpu"], None));
    mint("warm").await.expect("a runner");

    // Suspended: the cached token stops working, the next one is refused as suspended, and lifting it heals the binding.
    fake.with(|s| s.suspended = true);
    assert!(matches!(
        mint("a").await,
        Err(MintRefused::GitHub {
            status: Some(401),
            ..
        })
    ));
    assert!(matches!(
        mint("b").await,
        Err(MintRefused::Suspended { .. })
    ));
    assert_eq!(signer.bindings()[0].status, Status::Suspended);
    fake.with(|s| s.suspended = false);
    mint("c")
        .await
        .expect("a runner once the suspension is lifted");
    assert_eq!(signer.bindings()[0].status, Status::Active);

    // Uninstalled on GitHub.
    fake.with(|s| s.installed = false);
    assert!(mint("d").await.is_err());
    match mint("e").await {
        Err(MintRefused::Revoked { why }) => assert!(why.contains("uninstalled"), "{why}"),
        other => panic!("an uninstalled App revokes the binding: {other:?}"),
    }
    assert_eq!(signer.bindings()[0].status, Status::Revoked);
    assert!(matches!(mint("f").await, Err(MintRefused::Revoked { .. })));

    // Deleted on GitHub: a fresh binding on a fresh fake.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = self::signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    signer
        .mint(mint_request(&id, "warm", &["gpu"], None))
        .await
        .expect("a runner");
    fake.with(|s| s.app_deleted = true);
    assert!(
        signer
            .mint(mint_request(&id, "a", &["gpu"], None))
            .await
            .is_err()
    );
    match signer.mint(mint_request(&id, "b", &["gpu"], None)).await {
        Err(MintRefused::Revoked { why }) => assert!(why.contains("deleted"), "{why}"),
        other => panic!("a deleted App revokes the binding: {other:?}"),
    }
    let kinds: Vec<String> = signer
        .audit_log(None, 100)
        .iter()
        .map(|l| l["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"status".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnecting_an_app_uninstalls_it_deletes_its_key_and_links_to_deleting_it() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    let key = scratch.0.join("github/keys").join(format!("{id}.pem"));
    assert!(key.exists());

    let binding = signer
        .revoke(&id, "page:passkey-1")
        .await
        .expect("disconnected");
    assert_eq!(binding.status, Status::Revoked);
    let why = binding.why.unwrap();
    assert!(why.contains("uninstalled"), "{why}");
    assert!(
        why.contains(&format!(
            "/organizations/acme/settings/apps/{APP_SLUG}/advanced"
        )),
        "{why}"
    );
    let deleted = fake.seen("DELETE", &format!("/api/app/installations/{INSTALLATION}"));
    assert_eq!(deleted.len(), 1);
    assert!(!fake.with(|s| s.installed));
    assert!(!key.exists(), "the App's key is deleted");
    assert!(matches!(
        signer
            .mint(mint_request(&id, "after", &["gpu"], None))
            .await,
        Err(MintRefused::Revoked { .. })
    ));
    let last = signer.audit_log(Some(&id), 100);
    assert!(
        last.iter()
            .any(|l| l["kind"] == "revoke" && l["caller"] == "page:passkey-1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plan_without_runner_groups_falls_back_to_the_default_group_and_an_existing_group_is_adopted()
 {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.group_create_refusal = Some(403));
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    let group = signer.bindings()[0].runner_group.clone().unwrap();
    assert_eq!(group.id, 1);
    assert!(group.warning.unwrap().contains("default group"));
    let minted = signer
        .mint(mint_request(&id, "r", &["gpu"], None))
        .await
        .expect("a runner");
    assert_eq!(minted.runner_group_id, Some(1));

    // A group of Arca's name that holds no repository (the state an earlier connection left) is Arca's, and given
    // exactly the installation's repositories.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.groups.push(
            json!({ "id": 9, "name": RUNNER_GROUP_NAME, "default": false, "visibility": "selected" }),
        );
        s.group_repositories = vec![(9, json!([]))];
    });
    let signer = self::signer(&scratch, &fake);
    installed_app(&signer, &fake).await;
    let group = signer.bindings()[0].runner_group.clone().unwrap();
    assert_eq!((group.id, group.created_by_arca), (9, true));
    assert_eq!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/9")[0].body["visibility"],
        "selected"
    );
    assert_eq!(fake.group_repositories(9), Some(json!([11, 12])));
}

// ---- Issue 328: an installation that lists no repository ----------------------------------------------------------

/// What `github_bindings` answers on `signer`, checked against the verb's schema, which names the group's access.
async fn listed_bindings(signer: &Signer) -> Value {
    let handlers: std::collections::HashMap<&str, Arc<dyn arca_mcp::ToolHandler>> =
        arca::github_handlers::handlers(signer)
            .into_iter()
            .collect();
    let handler = Arc::clone(&handlers["github_bindings"]);
    let caller = Caller {
        token_id: "viewer".to_string(),
        scope: arca_api::verbs::Scope::Viewer,
        tools: None,
        limits: Default::default(),
    };
    let listed = tokio::task::spawn_blocking(move || handler.call(json!({}), &caller))
        .await
        .unwrap()
        .expect("the bindings");
    let schema = (arca_api::verb("github_bindings").expect("the verb").result)();
    let access = schema
        .pointer("/properties/bindings/items/properties/runnerGroup/properties/access/enum")
        .expect("the schema names the group's access");
    assert_eq!(
        access,
        &json!(["all-private", "selected", "unknown", "none", "unchecked"]),
        "{schema}"
    );
    arca_api::conforms(&schema, &listed).unwrap_or_else(|why| panic!("{why}: {listed}"));
    listed
}

/// The last `sync` line of the audit log.
fn last_sync(signer: &Signer, id: &str) -> Value {
    signer
        .audit_log(Some(id), 1000)
        .into_iter()
        .rev()
        .find(|line| line["kind"] == "sync")
        .expect("a sync line")
}

fn sync_lines(signer: &Signer, id: &str) -> usize {
    signer
        .audit_log(Some(id), 1000)
        .iter()
        .filter(|line| line["kind"] == "sync")
        .count()
}

/// Sets Arca's group `id` on the fake GitHub to `selected` with `repositories`, as an owner does on GitHub.
fn narrow_on_github(fake: &Fake, id: u64, repositories: Value) {
    fake.with(|s| {
        for group in s.groups.iter_mut().filter(|group| group["id"] == id) {
            group["visibility"] = json!("selected");
        }
        s.group_repositories.retain(|(group, _)| *group != id);
        s.group_repositories.push((id, repositories));
    });
}

const ALL_PRIVATE: &str = "Every private and internal repository of acme may send jobs here. Narrow it in the \
                           runner group on GitHub.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_installation_listing_no_repository_opens_arcas_group_to_every_private_repository() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    // The App asks only for the organisation permission, so GitHub offers no repository choice and lists none.
    fake.with(|s| s.repositories.clear());
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;

    let made = fake.seen("POST", "/api/orgs/acme/actions/runner-groups");
    assert_eq!(
        made[0].body,
        json!({
            "name": RUNNER_GROUP_NAME,
            "visibility": "all",
            "allows_public_repositories": false,
        })
    );
    assert!(
        fake.seen(
            "PUT",
            "/api/orgs/acme/actions/runner-groups/20/repositories"
        )
        .is_empty()
    );
    let group = fake.group(20).expect("the group on GitHub");
    assert_eq!(
        (&group["visibility"], &group["allows_public_repositories"]),
        (&json!("all"), &json!(false))
    );
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.status, Status::Active);
    assert_eq!(
        binding.runner_group.as_ref().unwrap().access,
        Some(GroupAccess::AllPrivate)
    );
    assert_eq!(binding.group_access(), Some(GroupAccess::AllPrivate));
    assert_eq!(binding.group_access_words().as_deref(), Some(ALL_PRIVATE));
    let sync = last_sync(&signer, &id);
    assert_eq!(sync["runnerGroupAccess"], "all-private", "{sync}");
    assert_eq!(sync["words"], ALL_PRIVATE, "{sync}");
    let listed = listed_bindings(&signer).await;
    assert_eq!(
        listed["bindings"][0]["runnerGroup"]["access"],
        "all-private"
    );
    assert_eq!(
        listed["bindings"][0]["runnerGroup"]["accessWords"],
        ALL_PRIVATE
    );

    // L4/L6: the owner narrows the group on GitHub, as the words ask; GitHub's return and a pool's sign-in leave it,
    // and a sync that changes nothing writes no line.
    let lines = sync_lines(&signer, &id);
    narrow_on_github(&fake, 20, json!([11]));
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    assert!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/20")
            .is_empty()
    );
    assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
    assert_eq!(fake.group_repositories(20), Some(json!([11])));
    assert_eq!(sync_lines(&signer, &id), lines);

    // The owner renamed the group on GitHub; the installation is then given a repository: the group is set to
    // exactly that one, under the name GitHub has for it.
    fake.with(|s| {
        for group in s.groups.iter_mut().filter(|group| group["id"] == 20) {
            group["name"] = json!("Our runners");
        }
        s.repositories = vec![(11, "acme/web".to_string())];
    });
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    let patched = fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/20");
    assert_eq!(
        patched.last().expect("a PATCH").body,
        json!({
            "name": "Our runners",
            "visibility": "selected",
            "allows_public_repositories": false,
        })
    );
    assert_eq!(fake.group(20).unwrap()["name"], "Our runners");
    assert_eq!(fake.group_repositories(20), Some(json!([11])));
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Selected));
    let words = binding.group_access_words().unwrap();
    assert!(words.contains("acme/web"), "{words}");
    assert_eq!(last_sync(&signer, &id)["runnerGroupAccess"], "selected");

    // L4: the owner narrows a `selected` group on GitHub; while the installation's list is unchanged, no sync puts
    // the list back.
    let puts = fake
        .seen(
            "PUT",
            "/api/orgs/acme/actions/runner-groups/20/repositories",
        )
        .len();
    narrow_on_github(&fake, 20, json!([]));
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    assert_eq!(
        fake.seen(
            "PUT",
            "/api/orgs/acme/actions/runner-groups/20/repositories"
        )
        .len(),
        puts
    );
    assert_eq!(fake.group_repositories(20), Some(json!([])));

    // L3: the owner removes the installation's last repository: the group stays `selected`, now with none, and reads
    // as open to no repository. It is not widened.
    fake.with(|s| s.repositories.clear());
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    let patches = fake
        .seen("PATCH", "/api/orgs/acme/actions/runner-groups/20")
        .len();
    assert_eq!(patches, 1, "only the PATCH to selected");
    assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
    assert_eq!(fake.group_repositories(20), Some(json!([])));
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Selected));
    let words = binding.group_access_words().unwrap();
    assert!(words.contains("open to no repository"), "{words}");
    let listed = listed_bindings(&signer).await;
    assert_eq!(listed["bindings"][0]["runnerGroup"]["access"], "selected");
    assert_eq!(listed["bindings"][0]["why"], words);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_left_open_to_no_repository_is_shown_so_and_repaired_by_the_pools_start_and_by_refresh()
 {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.repositories.clear());
    let id = installed_app(&signer(&scratch, &fake), &fake).await;
    // The state Arca before issue 328 left: its own group open to no repository, and no access in the record.
    let record = scratch.0.join("github/bindings").join(format!("{id}.json"));
    let break_it = || {
        narrow_on_github(&fake, 20, json!([]));
        let mut stored: Value =
            serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
        stored["runnerGroup"]
            .as_object_mut()
            .unwrap()
            .remove("access");
        fs::write(&record, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
        signer(&scratch, &fake)
    };

    let signer = break_it();
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.status, Status::Active);
    assert_eq!(binding.group_access(), Some(GroupAccess::NoRepository));
    let words = binding.group_access_words().unwrap();
    assert!(words.contains("open to no repository"), "{words}");
    let listed = listed_bindings(&signer).await;
    assert_eq!(listed["bindings"][0]["runnerGroup"]["access"], "none");
    assert_eq!(listed["bindings"][0]["runnerGroup"]["accessWords"], words);
    // Never shown as simply active: the answer says why.
    assert_eq!(listed["bindings"][0]["why"], words);

    // The pool's start (its session's sign-in reads the group) repairs it, with no owner step.
    let access = arca::runners::SignerAccess::new(signer.clone());
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert_eq!(fake.group(20).unwrap()["visibility"], "all");
    assert_eq!(fake.group(20).unwrap()["allows_public_repositories"], false);
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::AllPrivate)
    );
    let listed = listed_bindings(&signer).await;
    assert_eq!(
        listed["bindings"][0]["runnerGroup"]["access"],
        "all-private"
    );
    assert!(listed["bindings"][0].get("why").is_none(), "{listed}");

    // So does GitHub's later return (the `refresh` step).
    let signer = break_it();
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::NoRepository)
    );
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    assert_eq!(fake.group(20).unwrap()["visibility"], "all");
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::AllPrivate)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_arca_did_not_make_is_left_as_it_is_and_arcas_adopted_group_is_opened() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    // A plan without runner groups: the default group, which Arca did not make, is not changed.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.repositories.clear();
        s.group_create_refusal = Some(403);
    });
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    assert!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/1")
            .is_empty()
    );
    assert!(
        fake.seen("PUT", "/api/orgs/acme/actions/runner-groups/1/repositories")
            .is_empty()
    );
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Unknown));
    assert_eq!(
        binding.group_access_words().as_deref(),
        Some(
            "Arca leaves the runner group \"Default\" as GitHub has it, so it cannot tell which repositories of acme may send \
             jobs here. See the group's repository access on GitHub."
        )
    );
    assert_eq!(last_sync(&signer, &id)["runnerGroupAccess"], "unknown");
    assert_eq!(
        listed_bindings(&signer).await["bindings"][0]["runnerGroup"]["access"],
        "unknown"
    );

    // A group of Arca's name that holds no repository (what this bug, or a disconnect and reconnect, left) is
    // adopted as Arca's, and opened to every private repository.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.repositories.clear();
        s.groups.push(
            json!({ "id": 9, "name": RUNNER_GROUP_NAME, "default": false, "visibility": "selected" }),
        );
    });
    let signer = self::signer(&scratch, &fake);
    installed_app(&signer, &fake).await;
    assert_eq!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/9")[0].body,
        json!({
            "name": RUNNER_GROUP_NAME,
            "visibility": "all",
            "allows_public_repositories": false,
        })
    );
    assert!(
        fake.seen("PUT", "/api/orgs/acme/actions/runner-groups/9/repositories")
            .is_empty()
    );
    assert_eq!(fake.group(9).unwrap()["visibility"], "all");
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::AllPrivate)
    );

    // M2: an owner's own group of that name, with repositories, is not Arca's: not opened, not changed.
    for (visibility, repositories, access) in [
        ("selected", Some(json!([5])), GroupAccess::Unknown),
        ("all", None, GroupAccess::AllPrivate),
        ("private", None, GroupAccess::Unknown),
        // N3: no `allows_public_repositories` in GitHub's answer: not known, so not `all-private`.
        ("all-unsaid", None, GroupAccess::Unknown),
    ] {
        let scratch = Scratch::new();
        let fake = Fake::start("acme").await;
        fake.with(|s| {
            s.repositories.clear();
            let mut group = json!({
                "id": 9,
                "name": RUNNER_GROUP_NAME,
                "default": false,
                "visibility": visibility,
                "allows_public_repositories": false,
            });
            if visibility == "all-unsaid" {
                group["visibility"] = json!("all");
                group
                    .as_object_mut()
                    .unwrap()
                    .remove("allows_public_repositories");
            }
            s.groups.push(group);
            if let Some(list) = repositories.clone() {
                s.group_repositories = vec![(9, list)];
            }
        });
        let signer = self::signer(&scratch, &fake);
        let id = installed_app(&signer, &fake).await;
        signer
            .app_installed(INSTALLATION, None)
            .await
            .expect("GitHub's later return");
        arca::runners::SignerAccess::new(signer.clone())
            .runner_group(&id)
            .await
            .expect("read");
        assert!(
            fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/9")
                .is_empty(),
            "{visibility}"
        );
        assert!(
            fake.seen("PUT", "/api/orgs/acme/actions/runner-groups/9/repositories")
                .is_empty(),
            "{visibility}"
        );
        assert_eq!(
            fake.group(9).unwrap()["visibility"],
            visibility.trim_end_matches("-unsaid")
        );
        let group = signer.bindings()[0].runner_group.clone().unwrap();
        assert_eq!(
            (group.id, group.created_by_arca),
            (9, false),
            "{visibility}"
        );
        assert_eq!(
            signer.bindings()[0].group_access(),
            Some(access),
            "{visibility}"
        );
    }
}

/// M1, L5: a listing GitHub answers 2xx but not in its shape (not JSON, no `repositories` array, an entry without
/// `id` or `full_name`, an empty array that does not say `total_count: 0`), or a failing one, changes nothing on
/// GitHub; the binding keeps its record and says that the group is not checked yet, and why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_or_failing_listing_changes_nothing_and_says_the_group_is_not_checked() {
    use arca::github_signer::GroupAccess;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::Selected)
    );
    for (status, answer) in [
        (200, json!("<html>not JSON</html>")),
        (200, json!({ "total_count": 0 })),
        (200, json!({ "repositories": [] })),
        (200, json!({ "total_count": 2, "repositories": [] })),
        (
            200,
            json!({ "total_count": 1, "repositories": [{ "id": 11 }] }),
        ),
        (500, json!({ "message": "Server Error" })),
    ] {
        let calls = fake.with(|s| {
            s.listing = Some((status, answer.clone()));
            s.seen.len()
        });
        signer
            .app_installed(INSTALLATION, None)
            .await
            .expect("GitHub's later return is recorded");
        let after: Vec<String> = fake.with(|s| {
            s.seen[calls..]
                .iter()
                .map(|seen| format!("{} {}", seen.method, seen.path))
                .collect()
        });
        assert!(
            after
                .iter()
                .all(|call| !call.starts_with("PATCH") && !call.starts_with("PUT")),
            "{answer}: {after:?}"
        );
        assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
        assert_eq!(fake.group_repositories(20), Some(json!([11, 12])));
        let binding = signer.bindings()[0].clone();
        assert_eq!(
            binding.installation.as_ref().unwrap().repositories,
            vec!["acme/web", "acme/api"],
            "{answer}"
        );
        assert_eq!(binding.group_access(), Some(GroupAccess::Unchecked));
        let words = binding.group_access_words().unwrap();
        assert!(words.contains("not checked"), "{words}");
        let listed = listed_bindings(&signer).await;
        assert_eq!(listed["bindings"][0]["runnerGroup"]["access"], "unchecked");
        assert_eq!(listed["bindings"][0]["why"], words);
    }
    // A good listing again: checked, and as before.
    fake.with(|s| s.listing = None);
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Selected));
    assert!(binding.sync_error.is_none());
    let _ = id;
}

/// L1, L2: the sign-in's sync runs once for the classes that start together, not again within ten minutes, and a
/// failing or hanging one does not stop the sign-in: the group as recorded is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sign_in_sync_runs_once_and_a_failing_or_hanging_one_does_not_stop_the_sign_in() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let listings = |fake: &Fake| fake.seen("GET", "/api/installation/repositories").len();

    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    let access = arca::runners::SignerAccess::new(signer.clone());
    let before = listings(&fake);
    let lines = sync_lines(&signer, &id);
    let (a, b, c) = tokio::join!(
        access.runner_group(&id),
        access.runner_group(&id),
        access.runner_group(&id)
    );
    assert_eq!(
        (a.unwrap(), b.unwrap(), c.unwrap()),
        (Some(20), Some(20), Some(20))
    );
    assert_eq!(listings(&fake), before + 1, "one sync for three sign-ins");
    access.runner_group(&id).await.unwrap();
    assert_eq!(listings(&fake), before + 1, "none again within ten minutes");
    // Nothing changed, so nothing was written.
    assert_eq!(sync_lines(&signer, &id), lines);

    // A failing listing: the sign-in goes on with the recorded group, and the record says why it is not checked.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = self::signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    fake.with(|s| s.listing = Some((502, json!({ "message": "Bad Gateway" }))));
    let access = arca::runners::SignerAccess::new(signer.clone());
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::Unchecked)
    );

    // A hanging one: cut off at the deadline, and the sign-in goes on.
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = self::signer(&scratch, &fake).with_sign_in_deadline(Duration::from_millis(300));
    let id = installed_app(&signer, &fake).await;
    fake.with(|s| s.listing_delay_ms = 10_000);
    let access = arca::runners::SignerAccess::new(signer.clone());
    let started = Instant::now();
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Unchecked));
    assert!(
        binding
            .sync_error
            .as_deref()
            .unwrap_or("")
            .contains("did not answer"),
        "{:?}",
        binding.sync_error
    );
}

/// Waits, in steps of 20 ms and for at most five seconds, until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{what}: not within five seconds"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The record Arca 0.45 and older left for an installation that lists no repository: its own group, no access, the group
/// `selected` on GitHub with `repositories`. Answers the signer reopened on it.
async fn legacy_record(scratch: &Scratch, fake: &Fake, repositories: Value) -> (Signer, String) {
    fake.with(|s| s.repositories.clear());
    let id = installed_app(&signer(scratch, fake), fake).await;
    narrow_on_github(fake, 20, repositories);
    let record = scratch.0.join("github/bindings").join(format!("{id}.json"));
    let mut stored: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    stored["runnerGroup"]
        .as_object_mut()
        .unwrap()
        .remove("access");
    fs::write(&record, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
    fake.with(|s| s.seen.clear());
    (signer(scratch, fake), id)
}

/// N1 (1): the live record's first sync fails with a 502; the session's sign-in goes on, the words say when Arca checks
/// again, and the background retry opens the group with no new sign-in, `refresh` or restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_repair_is_retried_in_the_background_and_opens_the_group_without_a_new_sign_in() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!([])).await;
    let signer = signer.with_sync_retry(Duration::from_millis(100), Duration::from_millis(400));
    fake.with(|s| s.listing = Some((502, json!({ "message": "Bad Gateway" }))));
    let access = arca::runners::SignerAccess::new(signer.clone());
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Unchecked));
    let words = binding.group_access_words().unwrap();
    assert!(words.contains("checks again at "), "{words}");
    assert!(!words.contains("next sync"), "{words}");

    // GitHub answers again: the retry opens the group by itself.
    fake.with(|s| s.listing = None);
    until("the group opened by the retry", || {
        fake.group(20).unwrap()["visibility"] == "all"
    })
    .await;
    until("the record checked", || {
        signer.bindings()[0].group_access() == Some(GroupAccess::AllPrivate)
    })
    .await;
    assert!(signer.bindings()[0].sync_error.is_none());
}

/// N1 (2): an attempt cut off at the deadline does not count as the sync for ten minutes: the next sign-in syncs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_sync_does_not_hold_off_the_next_sign_in() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!([])).await;
    // No background retry within the test, so only the next sign-in can sync.
    let signer = signer
        .with_sign_in_deadline(Duration::from_millis(300))
        .with_sync_retry(Duration::from_secs(3600), Duration::from_secs(3600));
    fake.with(|s| s.listing_delay_ms = 10_000);
    let access = arca::runners::SignerAccess::new(signer.clone());
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::Unchecked)
    );
    fake.with(|s| s.listing_delay_ms = 0);
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(20));
    assert_eq!(fake.group(20).unwrap()["visibility"], "all");
    assert_eq!(
        signer.bindings()[0].group_access(),
        Some(GroupAccess::AllPrivate)
    );
}

/// N1 (3): the retries never run two at once (with sign-ins arriving meanwhile), and stop after a success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_background_retries_run_one_at_a_time_and_stop_after_a_success() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let listings = |fake: &Fake| fake.seen("GET", "/api/installation/repositories").len();
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!([])).await;
    let signer = signer.with_sync_retry(Duration::from_millis(30), Duration::from_millis(120));
    fake.with(|s| {
        s.listing = Some((502, json!({ "message": "Bad Gateway" })));
        s.listing_delay_ms = 40;
    });
    let access = arca::runners::SignerAccess::new(signer.clone());
    access.runner_group(&id).await.expect("read");
    for _ in 0..5 {
        let (a, b) = tokio::join!(access.runner_group(&id), access.runner_group(&id));
        a.unwrap();
        b.unwrap();
    }
    until("several retries", || listings(&fake) >= 8).await;
    fake.with(|s| s.listing = None);
    until("the record checked", || {
        signer.bindings()[0].group_access() == Some(GroupAccess::AllPrivate)
    })
    .await;
    let after = listings(&fake);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(listings(&fake), after, "no retry after the success");
    access.runner_group(&id).await.expect("read");
    assert_eq!(
        listings(&fake),
        after,
        "and the success holds off the next sign-in"
    );
    assert_eq!(
        fake.with(|s| s.listing_most_at_once),
        1,
        "never two at once"
    );
}

/// N2: the repair of an old record reads the group first. `selected` with repositories is the owner's own way around
/// the bug: left as it is, and recorded as selected with those repositories. (`selected` with none is opened: the repair
/// test above.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repair_of_an_old_record_keeps_the_owners_own_repositories() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!([5])).await;
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    signer
        .app_installed(INSTALLATION, None)
        .await
        .expect("GitHub's later return");
    assert!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/20")
            .is_empty()
    );
    assert!(
        fake.seen(
            "PUT",
            "/api/orgs/acme/actions/runner-groups/20/repositories"
        )
        .is_empty()
    );
    assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
    assert_eq!(fake.group_repositories(20), Some(json!([5])));
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Selected));
    assert!(!binding.open_to_none());
    let words = binding.group_access_words().unwrap();
    assert!(words.contains("acme/r5"), "{words}");
}

/// "HH:MM UTC" of a time in ms, as the words give the next check.
fn utc_minute(at: u64) -> String {
    let minute = at / 60_000 % 1440;
    format!("{:02}:{:02} UTC", minute / 60, minute % 60)
}

/// P1: the repair writes to a group only when it reads `selected` with no repository. Any other state it reads is
/// recorded as read and never written by a later sync either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_the_repair_reads_in_any_other_state_is_never_written() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    for (visibility, public) in [("private", false), ("all", true)] {
        let scratch = Scratch::new();
        let fake = Fake::start("acme").await;
        let (signer, id) = legacy_record(&scratch, &fake, json!([])).await;
        fake.with(|s| {
            let group = s.groups.iter_mut().find(|group| group["id"] == 20).unwrap();
            group["visibility"] = json!(visibility);
            group["allows_public_repositories"] = json!(public);
        });
        arca::runners::SignerAccess::new(signer.clone())
            .runner_group(&id)
            .await
            .expect("read");
        signer
            .app_installed(INSTALLATION, None)
            .await
            .expect("the second sync");
        assert!(
            fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/20")
                .is_empty(),
            "{visibility}: no PATCH on either sync"
        );
        assert!(
            fake.seen(
                "PUT",
                "/api/orgs/acme/actions/runner-groups/20/repositories"
            )
            .is_empty(),
            "{visibility}: no PUT on either sync"
        );
        let group = fake.group(20).unwrap();
        assert_eq!(
            (
                group["visibility"].clone(),
                group["allows_public_repositories"].clone()
            ),
            (json!(visibility), json!(public))
        );
        assert_eq!(
            signer.bindings()[0].group_access(),
            Some(GroupAccess::Unknown),
            "{visibility}"
        );
    }
}

/// P2: the classes that wait while a sync runs take its outcome, a failure included: one listing for the burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_classes_waiting_on_a_failing_sync_take_its_outcome() {
    let listings = |fake: &Fake| fake.seen("GET", "/api/installation/repositories").len();
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake)
        .with_sync_retry(Duration::from_secs(3600), Duration::from_secs(3600));
    let id = installed_app(&signer, &fake).await;
    fake.with(|s| {
        s.listing = Some((502, json!({ "message": "Bad Gateway" })));
        s.listing_delay_ms = 200;
    });
    let before = listings(&fake);
    let (a, b, c, d) = tokio::join!(
        signer.sync_for_sign_in(&id),
        signer.sync_for_sign_in(&id),
        signer.sync_for_sign_in(&id),
        signer.sync_for_sign_in(&id)
    );
    assert!(a.is_err() && b.is_err() && c.is_err() && d.is_err());
    assert_eq!(listings(&fake), before + 1, "one listing for four classes");
}

/// P3: the record keeps the time of the next check and the words give it; after a restart the node starts the retry
/// itself, so that promise holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_sync_is_retried_after_a_restart_and_the_words_give_its_time() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!([])).await;
    let signer = signer.with_sync_retry(Duration::from_secs(3600), Duration::from_secs(3600));
    fake.with(|s| s.listing = Some((502, json!({ "message": "Bad Gateway" }))));
    let started = clock()();
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    let binding = signer.bindings()[0].clone();
    let at = binding.sync_retry_at.expect("the next check");
    assert!(
        (started + 3_600_000..=clock()() + 3_600_000).contains(&at),
        "{at}"
    );
    let words = binding.group_access_words().unwrap();
    assert!(
        words.contains(&format!("Arca checks again at {}.", utc_minute(at))),
        "{words}"
    );

    // The node stops with the next check stored, due soon; it starts again and keeps that promise by itself.
    let record = scratch.0.join("github/bindings").join(format!("{id}.json"));
    let mut stored: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    stored["syncRetryAt"] = json!(clock()() + 100);
    fs::write(&record, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
    fake.with(|s| s.listing = None);
    let signer = self::signer(&scratch, &fake)
        .with_sync_retry(Duration::from_millis(100), Duration::from_millis(400));
    signer.resume_retries();
    until("the group opened after the restart", || {
        fake.group(20).unwrap()["visibility"] == "all"
    })
    .await;
    until("the record checked", || {
        signer.bindings()[0].group_access() == Some(GroupAccess::AllPrivate)
    })
    .await;
    assert!(signer.bindings()[0].sync_retry_at.is_none());
}

/// P3, P5: a disconnected connection promises no next check, runs no retry, and its record is not written again (a
/// failure that lands after the disconnect included), not after a restart either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disconnected_connection_promises_no_check_and_is_not_written_again() {
    use arca_runners::Access as _;
    let listings = |fake: &Fake| fake.seen("GET", "/api/installation/repositories").len();
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake)
        .with_sync_retry(Duration::from_millis(150), Duration::from_millis(150));
    let id = installed_app(&signer, &fake).await;
    fake.with(|s| s.listing = Some((502, json!({ "message": "Bad Gateway" }))));
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    // A sync in flight when the owner disconnects: its failure lands after the disconnect.
    fake.with(|s| {
        s.listing = Some((500, json!({ "message": "Server Error" })));
        s.listing_delay_ms = 300;
    });
    let in_flight = tokio::spawn({
        let signer = signer.clone();
        let id = id.clone();
        async move { signer.sync_for_sign_in(&id).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    signer
        .revoke(&id, "page:passkey-1")
        .await
        .expect("disconnected");
    let record = scratch.0.join("github/bindings").join(format!("{id}.json"));
    let written = fs::read_to_string(&record).unwrap();
    assert!(in_flight.await.unwrap().is_err());
    let words = signer.bindings()[0].group_access_words().unwrap();
    assert!(!words.contains("checks again"), "{words}");
    let before = listings(&fake);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(listings(&fake), before, "no retry after the disconnect");
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        written,
        "nothing written"
    );
    let restarted = self::signer(&scratch, &fake)
        .with_sync_retry(Duration::from_millis(50), Duration::from_millis(50));
    restarted.resume_retries();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(listings(&fake), before, "none after a restart");
}

/// P6: a group listing that gives a count but no repository array is not in GitHub's shape: it changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_listing_with_a_count_but_no_repositories_changes_nothing() {
    use arca::github_signer::GroupAccess;
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let (signer, id) = legacy_record(&scratch, &fake, json!({ "total_count": 3 })).await;
    arca::runners::SignerAccess::new(signer.clone())
        .runner_group(&id)
        .await
        .expect("read");
    let binding = signer.bindings()[0].clone();
    assert_eq!(binding.group_access(), Some(GroupAccess::Unchecked));
    assert!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/20")
            .is_empty()
    );
    assert_eq!(fake.group(20).unwrap()["visibility"], "selected");
}

// ---- Push mode ----------------------------------------------------------------------------------------------------

/// A configuration shaped like GitHub's `encoded_jit_config`.
fn pushed_config(agent: u64, name: &str) -> String {
    let runner =
        STANDARD.encode(json!({ "agentId": agent, "agentName": name, "poolId": 7 }).to_string());
    STANDARD.encode(
        json!({ ".runner": runner, ".credentials": "Y3JlZA==", ".credentials_rsaparams": "a2V5" })
            .to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_configurations_are_taken_with_the_intake_token_and_handed_out_once() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let (binding, token) = signer
        .push_create("acme", &[labels(&["linux", "x64"])], "page:passkey-1")
        .expect("a push connection");
    assert_eq!(binding.mode, Mode::Push);
    assert!(token.expose().starts_with("arcaintake_"));
    assert!(matches!(
        signer
            .mint(mint_request(&binding.id, "x", &["linux", "x64"], None))
            .await,
        Err(MintRefused::Empty)
    ));

    let body =
        json!({ "encodedJitConfig": pushed_config(77, "pushed-1"), "labels": ["x64", "linux"] });
    assert_eq!(
        signer
            .intake(&binding.id, "arcaintake_wrong", &body)
            .unwrap_err()
            .status,
        401
    );
    let wrong_labels =
        json!({ "encodedJitConfig": pushed_config(78, "pushed-2"), "labels": ["gpu"] });
    assert_eq!(
        signer
            .intake(&binding.id, token.expose(), &wrong_labels)
            .unwrap_err()
            .status,
        422
    );
    assert_eq!(
        signer
            .intake(&binding.id, token.expose(), &json!({}))
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(signer.intake(&binding.id, token.expose(), &body), Ok(1));

    // Through the kernel's HTTP route too: a browser's request is refused, a bearer's is queued.
    let router = arca::github_page::intake_router(signer.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let post = |origin: bool| {
        let body = json!({ "encodedJitConfig": pushed_config(79, "pushed-3"), "labels": ["linux", "x64"] }).to_string();
        let path = format!("/github/intake/{}", binding.id);
        let token = token.expose().to_string();
        async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            let origin = if origin {
                "Origin: https://evil.example.com\r\n"
            } else {
                ""
            };
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: arca.example.com\r\nAuthorization: Bearer {token}\r\n{origin}\
                 Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            answer
        }
    };
    assert!(post(true).await.starts_with("HTTP/1.1 403"));
    let answer = post(false).await;
    assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
    assert!(answer.contains(r#""queued":2"#), "{answer}");

    let first = signer
        .mint(mint_request(
            &binding.id,
            "ignored",
            &["linux", "x64"],
            None,
        ))
        .await
        .expect("the first pushed configuration");
    assert_eq!(
        (first.runner_id, first.runner_name.as_str()),
        (77, "pushed-1")
    );
    assert_eq!(
        first.encoded_jit_config.expose(),
        pushed_config(77, "pushed-1")
    );
    let second = signer
        .mint(mint_request(
            &binding.id,
            "ignored",
            &["linux", "x64"],
            None,
        ))
        .await
        .unwrap();
    assert_eq!(second.runner_id, 79);
    assert!(matches!(
        signer
            .mint(mint_request(
                &binding.id,
                "ignored",
                &["linux", "x64"],
                None
            ))
            .await,
        Err(MintRefused::Empty)
    ));

    signer
        .intake(&binding.id, token.expose(), &body)
        .expect("one more waits");
    signer
        .revoke(&binding.id, "page:passkey-1")
        .await
        .expect("disconnected");
    assert_eq!(
        signer
            .intake(&binding.id, token.expose(), &body)
            .unwrap_err()
            .status,
        401
    );
    assert!(
        !scratch.0.join("github/intake").join(&binding.id).exists(),
        "the waiting configurations are deleted"
    );
    let audit = signer.audit_log(Some(&binding.id), 100);
    assert_eq!(audit.iter().filter(|l| l["kind"] == "intake").count(), 3);
    for (path, text) in shown_files(&scratch.0) {
        assert!(
            !text.contains(token.expose()),
            "{} holds the intake token",
            path.display()
        );
        assert!(
            !text.contains(&pushed_config(77, "pushed-1")),
            "{} holds a configuration",
            path.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_flow_connection_mints_nothing() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.authorized = true);
    let signer = signer(&scratch, &fake);
    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 1))
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    let binding = done.binding.unwrap();
    assert_eq!(
        signer
            .mint(mint_request(&binding, "x", &["linux", "big-box"], None))
            .await
            .unwrap_err(),
        MintRefused::NotMintable
    );
    assert_eq!(
        signer
            .mint(mint_request("gh-nope", "x", &["linux"], None))
            .await
            .unwrap_err(),
        MintRefused::UnknownBinding
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_github_does_not_confirm_deleted_is_said_plainly_with_where_to_revoke_it() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| s.authorized = true);
    let signer = Signer::open(&scratch.0, fake.endpoints(), clock())
        .expect("the signer opens")
        .with_runner_key_bits(1024);
    // A rotated or mistyped secret: GitHub answers 404 to the deletion.
    signer
        .set_oauth_app(CLIENT_ID, &Secret::new("an-old-secret-0123456789"))
        .expect("the OAuth App is set");

    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 1))
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    assert!(!fake.with(|s| s.grant_deleted));
    let texts: Vec<&str> = done.steps.iter().map(|step| step.text.as_str()).collect();
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("The grant is deleted")),
        "{texts:#?}"
    );
    let refused = done
        .steps
        .iter()
        .find(|step| step.text.contains("NOT deleted"))
        .expect("said plainly");
    assert!(!refused.ok);
    assert!(refused.text.contains("404"), "{}", refused.text);
    assert!(
        refused.text.contains(arca::github_oauth::REVOKE_URL),
        "{}",
        refused.text
    );
    let last = done.steps.last().unwrap();
    assert!(last.text.contains("still in place"), "{}", last.text);
    assert!(!last.text.contains("holds no access"));
    let line = signer
        .audit_log(None, 100)
        .into_iter()
        .find(|line| line["kind"] == "grantDeleted")
        .expect("the deletion is logged");
    assert_eq!(
        (line["ok"].clone(), line["status"].clone()),
        (json!(false), json!(404))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_github_is_asked_again_and_slow_down_is_obeyed_while_polling() {
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.authorized = true;
        s.poll_script = vec!["503", "html", "slow_down"];
    });
    let signer = signer(&scratch, &fake);
    let started = Instant::now();
    let flow = signer
        .oauth_start(request("acme", Some("ci-pool"), 1))
        .await
        .unwrap();
    let done = settled(&signer, &flow.id).await;
    assert_eq!(done.state, FlowState::Done, "{:#?}", done.steps);
    assert_eq!(fake.seen("POST", "/web/login/oauth/access_token").len(), 4);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the poll after slow_down waited GitHub's interval"
    );
    assert!(fake.with(|s| s.grant_deleted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_pool_signs_in_and_mints_through_the_signer_inside_its_group() {
    use arca_runners::Access as _;
    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    let signer = signer(&scratch, &fake);
    let id = installed_app(&signer, &fake).await;
    let group = signer
        .binding(&id)
        .expect("the binding")
        .runner_group_id
        .expect("the group the App made");
    fake.with(|s| s.scale_sets = vec![(7, group), (8, group + 1)]);
    let access = arca::runners::SignerAccess::new(signer.clone());

    // The pool's scale sets go into the group the connection made.
    assert_eq!(access.runner_group(&id).await.expect("read"), Some(group));
    let connection = access.actions_connection(&id).await.expect("a connection");
    assert!(
        connection.service_url.contains("/tenant"),
        "{}",
        connection.service_url
    );

    let request = |name: &str, scale_set: u64, set: &[&str]| arca_runners::MintRequest {
        binding: id.clone(),
        scale_set_id: scale_set,
        runner_name: name.to_string(),
        labels: labels(set),
        work_folder: "/home/runner/work".to_string(),
        caller: "runner-pool:acme".to_string(),
    };
    let minted = access
        .mint(request("acme-0-00000001", 7, &["x64", "linux"]))
        .await
        .expect("a runner in the group's scale set");
    assert!(minted.runner_id > 0);
    assert!(minted.encoded_jit_config.expose().starts_with(JIT));
    // The work folder reached GitHub as the pool asked, and the configuration is no text the pool prints.
    let folder = fake.with(|s| s.runners.last().map(|runner| runner["workFolder"].clone()));
    assert_eq!(folder, Some(json!("/home/runner/work")));
    assert!(!format!("{:?}", minted.encoded_jit_config).contains(JIT));

    // A scale set outside the connection's group and labels outside its sets are refused, and neither is a sign-in failure.
    let outside = access
        .mint(request("acme-0-00000002", 8, &["linux", "x64"]))
        .await
        .expect_err("a scale set of another group");
    assert_eq!(outside.signin(), None, "{outside}");
    let labels_refused = access
        .mint(request("acme-0-00000003", 7, &["acme-4vcpu-ubuntu-2404"]))
        .await
        .expect_err("labels outside the connection's sets");
    assert_eq!(labels_refused.signin(), None, "{labels_refused}");

    // The job the runner took joins its mint in the signer's audit log.
    access
        .attach_job(
            minted.mint_id,
            arca_runners::JobRef {
                repo: "acme/omni".to_string(),
                run_id: Some(41),
                job_id: None,
                job_name: Some("plugin-gate".to_string()),
                workflow_ref: Some(
                    "acme/omni/.github/workflows/ci.yml@refs/heads/main".to_string(),
                ),
            },
        )
        .await;
    let audit = signer.audit_log(Some(&id), 100);
    assert!(audit.iter().any(|line| line["kind"] == "mint"
        && line["caller"] == "runner-pool:acme"
        && line["outcome"] == "minted"));
    // A job line names its mint, not the binding, so it is read from the whole log.
    assert!(
        signer
            .audit_log(None, 100)
            .iter()
            .any(|line| line["kind"] == "job" && line["mintId"] == json!(minted.mint_id))
    );

    // A connection that does not exist is no credential.
    let unknown = access
        .actions_connection("gh-nobody")
        .await
        .expect_err("no such connection");
    assert_eq!(
        unknown.signin(),
        Some(arca_runners::SignInReason::NoCredential)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_15_second_app_mints_one_repo_and_exact_permissions_without_changing_runners() {
    for app_slug in ["project-custom-repositories", "arca-repositories"] {
        let bot_login = format!("{}[bot]", app_slug.to_ascii_uppercase());
        let scratch = Scratch::new();
        let fake = Fake::start("acme").await;
        fake.with(|s| {
            s.repository_app = true;
            s.repository_app_slug = app_slug.into();
            s.repository_bot_login = Some(bot_login.clone());
        });
        let signer = signer(&scratch, &fake);
        let runner = signer
            .app_begin(
                "acme",
                &[vec!["linux".into()]],
                "https://arca.example",
                "page",
            )
            .unwrap();
        let manifest: Value = serde_json::from_str(&runner.manifest).unwrap();
        assert_eq!(
            manifest["default_permissions"],
            json!({"organization_self_hosted_runners":"write"})
        );
        let start = signer
            .repository_app_begin("acme", "https://arca.example", "page")
            .unwrap();
        let manifest: Value = serde_json::from_str(&start.manifest).unwrap();
        let permissions = json!({"contents":"write","pull_requests":"write","metadata":"read"});
        assert_eq!(
            manifest["name"],
            format!("Arca repositories {}", &start.state[..6])
        );
        let another = signer
            .repository_app_begin("acme", "https://arca.example", "page")
            .unwrap();
        let another_manifest: Value = serde_json::from_str(&another.manifest).unwrap();
        assert_ne!(manifest["name"], another_manifest["name"]);
        assert_eq!(manifest["default_permissions"], permissions);
        assert_eq!(manifest["public"], false);
        assert_eq!(manifest["hook_attributes"]["active"], false);
        assert_eq!(manifest["default_events"], json!([]));
        signer
            .app_created(MANIFEST_CODE, &start.state)
            .await
            .unwrap();
        fake.with(|s| s.installed = true);
        let binding = signer
            .app_installed(INSTALLATION, Some(&start.state))
            .await
            .unwrap();
        assert!(
            fake.seen(
                "POST",
                &format!("/api/app/installations/{INSTALLATION}/access_tokens")
            )
            .is_empty(),
            "install must not mint an unrestricted token"
        );
        assert!(
            signer
                .bindings()
                .iter()
                .find(|b| b.id == binding)
                .unwrap()
                .repository_access
        );
        fake.with(|s| s.repository_bot_login = Some("unrelated-app[bot]".into()));
        assert!(
            signer
                .repository_credential("https://github.com/acme/api")
                .await
                .is_err()
        );
        assert!(
            fake.seen(
                "POST",
                &format!("/api/app/installations/{INSTALLATION}/access_tokens")
            )
            .is_empty()
        );
        fake.with(|s| s.repository_bot_login = Some(bot_login.clone()));
        for repo in ["api", "web"] {
            let credential = signer
                .repository_credential(&format!("https://github.com/acme/{repo}"))
                .await
                .unwrap();
            assert_eq!(credential.bot_id, 7007, "bot user id, never App id");
            assert_eq!(credential.bot_login, bot_login);
            let record = signer
                .bindings()
                .into_iter()
                .find(|b| b.id == binding)
                .unwrap();
            assert_eq!(
                serde_json::to_value(record.app.unwrap()).unwrap()["botLogin"],
                bot_login
            );
            assert_eq!(credential.token.expose(), fake::INSTALL_TOKEN);
        }
        assert!(
            signer
                .repository_credential("https://github.com/acme/unselected")
                .await
                .is_err()
        );
        assert!(
            signer
                .repository_credential("https://other.example/acme/api")
                .await
                .is_err()
        );
        for bad in [
            "http://github.com/acme/api",
            "https://user@github.com/acme/api",
            "https://github.com/acme/api/extra",
            "https://github.com/acme/api?token=bad",
            "https://github.com/acme/..",
        ] {
            assert!(signer.repository_credential(bad).await.is_err());
        }
        let requests = fake.seen(
            "POST",
            &format!("/api/app/installations/{INSTALLATION}/access_tokens"),
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body,
            json!({"repositories":["api"],"permissions":permissions})
        );
        assert_eq!(
            requests[1].body,
            json!({"repositories":["web"],"permissions":permissions})
        );
        assert!(
            fake.seen("POST", "/api/orgs/acme/actions/runner-groups")
                .is_empty()
        );
        assert!(matches!(
            signer
                .mint(MintRequest {
                    binding: binding.clone(),
                    runner_name: "test".into(),
                    labels: vec![],
                    work_folder: None,
                    scale_set_id: None,
                    job: None,
                    caller: "test".into()
                })
                .await,
            Err(MintRefused::NotMintable)
        ));
        // The purpose survives an Arca restart; no installation-token cache broadens it.
        let reopened = Signer::open(&scratch.0, fake.endpoints(), clock()).unwrap();
        reopened
            .repository_credential("https://github.com/acme/api.git")
            .await
            .unwrap();
        let requests = fake.seen(
            "POST",
            &format!("/api/app/installations/{INSTALLATION}/access_tokens"),
        );
        assert_eq!(
            requests[2].body,
            json!({"repositories":["api"],"permissions":permissions})
        );
        assert_eq!(
            fake.seen("GET", &format!("/api/users/{app_slug}%5Bbot%5D"))
                .len(),
            2,
            "verified bot metadata survives restart without unauthenticated lookup on every mint"
        );
        for (path, text) in shown_files(&scratch.0) {
            assert_no_secret(&path.display().to_string(), &text);
        }
    }
}
