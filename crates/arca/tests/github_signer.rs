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

    let scratch = Scratch::new();
    let fake = Fake::start("acme").await;
    fake.with(|s| {
        s.groups.push(
            json!({ "id": 9, "name": RUNNER_GROUP_NAME, "default": false, "visibility": "all" }),
        )
    });
    let signer = self::signer(&scratch, &fake);
    installed_app(&signer, &fake).await;
    let group = signer.bindings()[0].runner_group.clone().unwrap();
    assert_eq!((group.id, group.created_by_arca), (9, true));
    assert_eq!(
        fake.seen("PATCH", "/api/orgs/acme/actions/runner-groups/9")[0].body["visibility"],
        "selected"
    );
    assert_eq!(
        fake.with(|s| s.group_repositories.clone()),
        vec![(9, json!([11, 12]))]
    );
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
