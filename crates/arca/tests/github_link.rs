//! The owner's GitHub page with a prepared link, and a linked node connected from it (issue 320), against the fake GitHub
//! (`tests/github/fake.rs`).
//!
//! Alpha serves the page; beta is a linked node with no page of its own: a kernel whose approval verifier trusts alpha's
//! owner passkey (synced peer keys, for alpha's page origin), behind an MCP door on its own unix socket, with the relay
//! token alpha holds (doctor scope, its tool list naming `github_link`, `action_propose`, `plan_list` and
//! `action_approve`). The page drives beta through `crate::linked` exactly as it does in production, and the owner's
//! passkey is a software passkey from a fixed seed. Beta begins a connection and disconnects one only from a `github_link`
//! proposal whose passkey assertion it checks itself; it converts GitHub's code itself, so the App's key is made into a
//! file on beta alone.

mod common;
#[path = "github/fake.rs"]
mod fake;
#[path = "../../arca-approval/tests/common/webauthn.rs"]
mod webauthn;

use std::collections::HashMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca::approve_page::{Page, SetupCodeDisplay};
use arca::config::LinkedHost;
use arca::github_api::Secret;
use arca::github_handlers::{LINK_VERB, handlers, link_handler};
use arca::github_page::{self, Ctx, Links};
use arca::github_signer::{Mode, Signer, Status};
use arca::handlers::{ActionExecutor, ActionSlot, Clock, plan_handlers_with};
use arca::kernel::Kernel;
use arca::linked::{self, RelayError};
use arca_api::{Scope, Verb};
use arca_approval::{
    ApprovalVerifier, FileNonces, FileSignCounts, OwnerKeys, PasskeyVerifier, PeerPasskeys,
    SshSigVerifier, normalize_set,
};
use arca_mcp::{CallError, Caller, Server, TokenStore, ToolHandler, mint_with_tools, serve_unix};
use axum::response::Response;
use serde_json::{Value, json};

use common::{NAMESPACE, Scratch, kernel_on_host, start_fake};
use fake::{APP_SLUG, CLIENT_ID, CLIENT_SECRET, Fake, INSTALLATION, MANIFEST_CODE, SECRETS};
use webauthn::{Assert, ORIGIN, SoftPasskey, passkey_approval};

/// The page host's own name.
const HERE: &str = "alpha";
/// The linked node's name, as `approval.linkedHosts` lists it.
const NODE: &str = "beta";
/// The host beta's approval statements name.
const BETA_HOST: &str = "beta.example";
/// The passkey session the page acts for.
const CALLER: &str = "page:passkey-1";
/// The longest any one test may take.
const BOUND: Duration = Duration::from_secs(60);
/// The tools of the relay token alpha holds for beta, as the operating guide mints it.
const RELAY_TOOLS: [&str; 4] = [
    "github_link",
    "action_propose",
    "plan_list",
    "action_approve",
];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as u64
}

fn clock() -> Clock {
    Arc::new(now_ms)
}

fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("a test file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("its mode");
}

/// Beta's `github_link` as its door serves it: each step it is asked is noted, and the answer of the first call of each
/// listed step is held back for a while after the step has run: an answer that comes late, once.
struct Late {
    inner: Arc<dyn ToolHandler>,
    delays: Mutex<Vec<(&'static str, Duration)>>,
    steps: Arc<Mutex<Vec<String>>>,
}

impl ToolHandler for Late {
    fn call(&self, args: Value, caller: &Caller) -> Result<Value, CallError> {
        self.steps
            .lock()
            .expect("the steps")
            .push(args["step"].as_str().unwrap_or("").to_string());
        let delay = {
            let mut delays = self.delays.lock().expect("the delays");
            let at = delays.iter().position(|(step, _)| args["step"] == *step);
            at.map(|at| delays.remove(at).1)
        };
        let answer = self.inner.call(args, caller);
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        answer
    }
}

/// How the rig is made.
struct Setup {
    /// The page origins beta trusts the owner's passkeys for.
    trusted: Vec<String>,
    /// The tools of alpha's relay token on beta.
    tools: Vec<String>,
    /// How long the page waits for a `github_link` step.
    wait: Option<Duration>,
    /// The first answer of each of these steps comes this late.
    late: Vec<(&'static str, Duration)>,
    /// Beta runs an Arca from before `github_link`.
    older: bool,
    /// The page reads a state-less return of one installation at most once a minute, as in production; otherwise the
    /// rig lets every return through.
    throttle: bool,
}

impl Setup {
    fn new(trusted: &[&str], tools: &[&str]) -> Self {
        Self {
            trusted: trusted.iter().map(|origin| origin.to_string()).collect(),
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
            wait: None,
            late: Vec::new(),
            older: false,
            throttle: false,
        }
    }
}

/// Alpha, the page host, and beta, the linked node, on one fake GitHub.
struct Rig {
    scratch: Scratch,
    _incus_scratch: Scratch,
    _incus: arca_incus::FakeIncus,
    fake: Fake,
    alpha: Signer,
    beta: Signer,
    beta_kernel: Arc<Mutex<Kernel>>,
    links: Links,
    beta_socket: PathBuf,
    beta_router: axum::Router,
    /// The owner's passkey, which alpha's page answers with and beta trusts.
    owner: SoftPasskey,
    /// Every `github_link` step beta was asked, in order.
    steps: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    /// Beta trusts the owner's passkeys for the page origins `trusted`, and alpha's relay token on beta names `tools`.
    async fn new(trusted: &[&str], tools: &[&str]) -> Self {
        Self::with(Setup::new(trusted, tools)).await
    }

    async fn with(setup: Setup) -> Self {
        let scratch = Scratch::new("ghlink");
        let incus_scratch = Scratch::new("ghlink-beta-kernel");
        let fake = Fake::start("acme").await;
        let alpha_state = scratch.path("alpha");
        let beta_state = scratch.path("beta");
        fs::create_dir(&alpha_state).expect("alpha's state");
        fs::create_dir(&beta_state).expect("beta's state");
        let alpha = Signer::open(&alpha_state, fake.endpoints(), clock()).expect("alpha's signer");
        alpha
            .set_oauth_app(CLIENT_ID, &Secret::new(CLIENT_SECRET))
            .expect("alpha's OAuth App");
        let beta = Signer::open(&beta_state, fake.endpoints(), clock()).expect("beta's signer");

        // Beta's kernel: no passkey of its own; it keeps alpha's owner passkey, synced, for the trusted page origins.
        let owner = SoftPasskey::from_seed(1);
        let peer = PeerPasskeys::new(
            incus_scratch.path("peer-passkeys"),
            HERE.to_string(),
            setup.trusted.clone(),
        );
        let lines = normalize_set(&[owner.line(NAMESPACE)]).expect("a key to sync");
        peer.write(&lines, now_ms(), None).expect("the synced keys");
        let passkeys = PasskeyVerifier::new(
            Vec::new(),
            FileSignCounts::open(&incus_scratch.path("beta-counts.jsonl")).expect("counts"),
        )
        .with_peer(peer);
        let verifier = ApprovalVerifier::new(
            BETA_HOST.to_string(),
            OwnerKeys::new(SshSigVerifier::new(Vec::new()), passkeys),
            FileNonces::open(&incus_scratch.path("beta-nonces.jsonl")).expect("nonces"),
        );
        let incus = start_fake(&incus_scratch);
        let beta_kernel = Arc::new(Mutex::new(
            kernel_on_host(&incus_scratch, &incus, verifier, now_ms(), None, BETA_HOST)
                .expect("beta's kernel opens"),
        ));

        // Beta's MCP door: the relay token alpha holds, an agent's doctor token with no list, and an operator token.
        let tokens_dir = scratch.path("beta-tokens");
        fs::create_dir(&tokens_dir).expect("the token directory");
        fs::set_permissions(&tokens_dir, fs::Permissions::from_mode(0o700)).expect("0700");
        for (name, scope, tools) in [
            ("relay", Scope::Doctor, Some(setup.tools.clone())),
            ("agent", Scope::Doctor, None),
            ("operator", Scope::Operator, None),
        ] {
            let (secret, hash) = mint_with_tools(scope, tools.as_deref());
            write_mode(&tokens_dir.join(format!("{name}.hash")), &hash, 0o400);
            write_mode(
                &scratch.path(&format!("{name}-to-beta.secret")),
                &format!("{secret}\n"),
                0o600,
            );
        }
        let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
        let link = link_handler(&beta, Arc::clone(&beta_kernel), setup.trusted.clone());
        let slot = ActionSlot::default();
        let verbs: &'static [Verb] = if setup.older {
            Box::leak(
                arca_api::verbs()
                    .iter()
                    .filter(|verb| verb.name != LINK_VERB)
                    .copied()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        } else {
            arca_api::verbs()
        };
        let mut server = Server::new(verbs, tokens);
        for (verb, handler) in plan_handlers_with(
            Arc::clone(&beta_kernel),
            clock(),
            None,
            None,
            Arc::clone(&slot),
        ) {
            server = server.handle(verb, handler);
        }
        let _ = slot.set(ActionExecutor::new([(LINK_VERB, Arc::clone(&link))]));
        let steps = Arc::new(Mutex::new(Vec::new()));
        if !setup.older {
            let served: Arc<dyn ToolHandler> = Arc::new(Late {
                inner: link,
                delays: Mutex::new(setup.late),
                steps: Arc::clone(&steps),
            });
            server = server.handle(LINK_VERB, served);
        }
        let beta_router = server.router();
        let beta_socket = scratch.path("beta.sock");
        let mut links = Links::over(vec![host(&scratch, &beta_socket, "relay")])
            .kept_in(scratch.path("alpha/github-linked-apps.json"));
        if let Some(wait) = setup.wait {
            links = links.with_wait(wait);
        }
        if !setup.throttle {
            links = links.with_reconcile_gap(Duration::ZERO);
        }
        let rig = Self {
            scratch,
            _incus_scratch: incus_scratch,
            _incus: incus,
            fake,
            alpha,
            beta,
            beta_kernel,
            links,
            beta_socket,
            beta_router,
            owner,
            steps,
        };
        rig.beta_up().await;
        rig
    }

    /// Beta's door answers on its socket.
    async fn beta_up(&self) {
        let router = self.beta_router.clone();
        let socket = self.beta_socket.clone();
        tokio::spawn(async move {
            let _ = serve_unix(router, &socket).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !self.beta_socket.exists() {
            assert!(Instant::now() < until, "beta's door started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Beta stops answering: nothing listens on its socket's path.
    fn beta_down(&self) {
        fs::remove_file(&self.beta_socket).expect("beta's socket is there");
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            signer: &self.alpha,
            origin: ORIGIN,
            caller: CALLER,
            here: HERE,
            links: &self.links,
            credentials: &[],
        }
    }

    /// Beta, reached with the token `name` holds.
    fn as_token(&self, name: &str) -> LinkedHost {
        host(&self.scratch, &self.beta_socket, name)
    }

    /// One call to beta's door with the token `name` holds, as the relay makes it.
    async fn direct(&self, name: &str, verb: &str, args: Value) -> Result<Value, RelayError> {
        let host = self.as_token(name);
        let verb = verb.to_string();
        tokio::task::spawn_blocking(move || linked::call(&host, &verb, args))
            .await
            .expect("the call ends")
    }

    fn calls(&self, method: &str, path: &str) -> usize {
        self.fake.seen(method, path).len()
    }

    fn conversions(&self) -> usize {
        self.calls(
            "POST",
            &format!("/api/app-manifests/{MANIFEST_CODE}/conversions"),
        )
    }

    /// The `github_link` cards waiting on beta.
    fn waiting_cards(&self) -> usize {
        self.beta_kernel
            .lock()
            .expect("beta's kernel")
            .action_proposals()
            .iter()
            .filter(|proposal| proposal.action == LINK_VERB)
            .count()
    }

    /// Every file of alpha's, as text.
    fn alpha_files(&self) -> Vec<(PathBuf, String)> {
        files(&self.scratch.path("alpha"))
    }

    /// The form with the owner's approval of the card its button prepares: the passkey's assertion over the statement
    /// the page answers.
    async fn approve(&self, form: &str) -> HashMap<String, String> {
        self.approve_with(form, &self.owner).await
    }

    async fn approve_with(&self, form: &str, key: &SoftPasskey) -> HashMap<String, String> {
        let mut form = query(form);
        let (status, body) = prepare(self, &form).await;
        assert_eq!(status, 200, "{body}");
        let statement = body["statement"].as_str().expect("a statement").to_string();
        let proof = key.assert(&Assert::over(NAMESPACE, statement.as_bytes()));
        form.insert(
            "plan".to_string(),
            body["plan"].as_str().expect("a plan").to_string(),
        );
        form.insert("approval".to_string(), passkey_approval(&statement, &proof));
        form
    }
}

fn host(scratch: &Scratch, socket: &Path, token: &str) -> LinkedHost {
    LinkedHost {
        name: NODE.to_string(),
        socket: socket.to_path_buf(),
        token_file: scratch.path(&format!("{token}-to-beta.secret")),
        incus_socket: None,
        ceph: None,
    }
}

fn files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push((path.clone(), fs::read_to_string(&path).unwrap_or_default()));
            }
        }
    }
    out
}

/// A response's status, `Location` and body.
async fn read(response: Response) -> (u16, Option<String>, String) {
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .expect("the body");
    (
        status,
        location,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

/// The prepare step's status and JSON answer.
async fn prepare(rig: &Rig, form: &HashMap<String, String>) -> (u16, Value) {
    let (status, _, body) = read(github_page::prepare(&rig.ctx(), form).await).await;
    (status, serde_json::from_str(&body).expect("a JSON answer"))
}

fn query(text: &str) -> HashMap<String, String> {
    github_page::parse_form(text)
}

/// The hidden `name` field's value in a page's form.
fn hidden(body: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let at = body.find(&marker).expect("the hidden field") + marker.len();
    let end = body[at..].find('"').expect("its end");
    body[at..at + end]
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The `state` in an address.
fn state_of(url: &str) -> String {
    let at = url.find("state=").expect("a state") + "state=".len();
    url[at..].split('&').next().expect("its value").to_string()
}

fn assert_no_secret(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{what} holds a credential");
    }
    assert!(!text.contains(MANIFEST_CODE), "{what} holds GitHub's code");
}

/// The form the page draws for the linked node's App, with runners in two label sets.
const BEGIN_FORM: &str = "node=beta&org=acme&labels=self-hosted%2C+linux%0D%0Agpu";

/// Begins an App flow on beta from the page, with the owner's one tap: the manifest's state.
async fn begin_on_beta(rig: &Rig) -> String {
    let form = rig.approve(BEGIN_FORM).await;
    let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
    assert_eq!(status, 200, "{body}");
    let manifest: Value = serde_json::from_str(&hidden(&body, "manifest")).expect("a manifest");
    // GitHub sends the browser back to the page the owner is on, never to beta.
    assert_eq!(
        manifest["redirect_url"],
        format!("{ORIGIN}/approve/github/created")
    );
    assert_eq!(
        manifest["setup_url"],
        format!("{ORIGIN}/approve/github/installed")
    );
    let action = body
        .split("action=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the form's action")
        .replace("&amp;", "&");
    state_of(&action)
}

async fn created(rig: &Rig, state: &str) -> (u16, Option<String>, String) {
    let q = query(&format!("code={MANIFEST_CODE}&state={state}"));
    read(github_page::created(&rig.ctx(), &q).await).await
}

async fn installed(rig: &Rig, state: &str) -> (u16, Option<String>, String) {
    let q = query(&format!(
        "installation_id={INSTALLATION}&setup_action=install&state={state}"
    ));
    read(github_page::installed(&rig.ctx(), &q, false).await).await
}

/// GitHub's return with no `state` (repositories changed, or an install finished after the hour), through the owner's
/// own browser, signed in on the page.
async fn installed_later(rig: &Rig, action: &str) -> (u16, Option<String>, String) {
    let q = query(&format!(
        "installation_id={INSTALLATION}&setup_action={action}"
    ));
    read(github_page::installed(&rig.ctx(), &q, true).await).await
}

/// A whole connection of beta from the page: begin, created, installed. Beta's binding.
async fn connect_beta(rig: &Rig) -> arca::github_signer::Binding {
    let state = begin_on_beta(rig).await;
    let (status, _, body) = created(rig, &state).await;
    assert_eq!(status, 303, "{body}");
    rig.fake.with(|s| s.installed = true);
    let (status, _, body) = installed(rig, &state).await;
    assert_eq!(status, 303, "{body}");
    rig.beta.bindings()[0].clone()
}

fn submit_buttons(body: &str) -> usize {
    body.matches("type=\"submit\"").count()
}

/// The prepared section of a page.
fn prepared_section(body: &str) -> &str {
    let at = body
        .find("<section id=\"prepared\"")
        .expect("the prepared section");
    let end = body[at..].find("</section>").expect("its end");
    &body[at..at + end]
}

fn refusal_code(error: &RelayError) -> &str {
    match error {
        RelayError::Refused { code, .. } => code,
        RelayError::Token => "Token",
        RelayError::Unreachable => "Unreachable",
        RelayError::Failed(_) => "Failed",
    }
}

// ---- The prepared link, this node ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepared_app_link_fills_the_form_and_posts_exactly_as_a_typed_one() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let q = query("mode=app&org=acme&labels=self-hosted%2Clinux%2Cx64%3Bgpu");
        let (status, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert_eq!(status, 200);
        // The values, editable, and one button; the node named in words.
        assert!(body.contains("value=\"acme\""), "{body}");
        assert!(
            body.contains(">self-hosted,linux,x64\ngpu</textarea>"),
            "{body}"
        );
        assert!(body.contains(&format!("action=\"{}\"", github_page::APP_PATH)));
        assert_eq!(submit_buttons(&body), 1, "{body}");
        assert!(body.contains(&format!("this node, {HERE}")), "{body}");
        assert!(!body.contains("name=\"node\""));
        // Nothing was begun by opening the link.
        assert!(rig.fake.with(|s| s.seen.is_empty()));

        // The form as the browser posts it: the textarea's lines.
        let form = query("org=acme&labels=self-hosted%2Clinux%2Cx64%0D%0Agpu");
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_eq!(status, 200, "{body}");
        let manifest: Value = serde_json::from_str(&hidden(&body, "manifest")).unwrap();
        assert_eq!(
            manifest["redirect_url"],
            format!("{ORIGIN}/approve/github/created")
        );

        // A value the link got wrong is refused in the typed form's own words.
        let typed =
            read(github_page::app_start(&rig.ctx(), &query("org=-acme&labels=linux")).await).await;
        assert_eq!(typed.0, 400);
        assert!(
            typed
                .2
                .contains("1 to 39 letters, digits and single hyphens")
        );
        let q = query("mode=app&org=-acme&labels=linux");
        let (_, _, page) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(page.contains("value=\"-acme\""));
        let linked =
            read(github_page::app_start(&rig.ctx(), &query("org=-acme&labels=linux")).await).await;
        assert_eq!(linked.2, typed.2);

        // A value is shown as text, never as markup.
        let q = query("mode=app&org=%22%3E%3Cb%3Ex&labels=a");
        let (_, _, page) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(page.contains("value=\"&quot;&gt;&lt;b&gt;x\""));
        assert!(!page.contains("\"><b>x"));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepared_link_fills_the_repository_push_and_sign_in_forms() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;

        // The workspace repositories App: the organisation, or a repository whose owner it is.
        for link in [
            "mode=repositories&org=acme",
            "mode=repository&repository=acme%2Fweb",
        ] {
            let (status, _, body) = read(github_page::view(&rig.ctx(), &query(link)).await).await;
            assert_eq!(status, 200);
            assert!(
                body.contains("name=\"purpose\" value=\"repositories\""),
                "{link}: {body}"
            );
            assert!(body.contains("value=\"acme\""), "{link}: {body}");
            assert_eq!(submit_buttons(&body), 1, "{link}");
        }
        let (status, _, body) =
            read(github_page::app_start(&rig.ctx(), &query("purpose=repositories&org=acme")).await)
                .await;
        assert_eq!(status, 200, "{body}");
        let manifest: Value = serde_json::from_str(&hidden(&body, "manifest")).unwrap();
        assert!(
            manifest["name"]
                .as_str()
                .unwrap()
                .starts_with("Arca repositories")
        );

        // Pushed configurations.
        let q = query("mode=push&org=acme&labels=linux%2Cx64%3Bgpu");
        let (_, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(
            body.contains(&format!("action=\"{}\"", github_page::PUSH_PATH)),
            "{body}"
        );
        assert!(body.contains(">linux,x64\ngpu</textarea>"), "{body}");
        assert_eq!(submit_buttons(&body), 1);

        // The sign-in with a code: group, labels and how many runners.
        let q = query("mode=oauth&org=acme&group=ci&labels=linux%2Cx64&runners=3");
        let (_, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(
            body.contains(&format!("action=\"{}\"", github_page::DEVICE_PATH)),
            "{body}"
        );
        assert!(body.contains("value=\"ci\""), "{body}");
        assert!(body.contains("value=\"linux,x64\""), "{body}");
        assert!(body.contains("value=\"3\""), "{body}");
        assert_eq!(submit_buttons(&body), 1);
        assert!(rig.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

// ---- A linked node -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_is_connected_from_this_page_with_one_passkey_tap_and_holds_the_key_alone() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;

        // The prepared link for the linked node names it and carries it in the form, whose one button is the approval.
        let q = query(&format!("mode=app&node={NODE}&org=acme&labels=linux%3Bgpu"));
        let (status, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(&format!("linked node {NODE}")), "{body}");
        let section = prepared_section(&body);
        assert!(
            section.contains(&format!("name=\"node\" value=\"{NODE}\"")),
            "{section}"
        );
        assert!(section.contains("data-approve"), "{section}");
        assert!(section.contains("with your passkey"), "{section}");
        assert_eq!(submit_buttons(section), 1, "{section}");
        // Opening the page asked beta nothing, and GitHub nothing.
        assert_eq!(rig.waiting_cards(), 0);
        assert!(rig.fake.with(|s| s.seen.is_empty()));

        let state = begin_on_beta(&rig).await;
        // The card the owner approved is spent on beta.
        assert_eq!(rig.waiting_cards(), 0);
        let (status, location, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let install = location.expect("the install screen");
        assert!(install.starts_with(&format!(
            "{}/web/apps/{APP_SLUG}/installations/new",
            rig.fake.base
        )));
        assert_eq!(rig.conversions(), 1);
        rig.fake.with(|s| s.installed = true);
        let (status, location, body) = installed(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let location = location.expect("back to the page");
        assert!(location.starts_with(&format!(
            "{}?node={NODE}&connected=gh-",
            github_page::PAGE_PATH
        )));

        // Beta holds the connection and its key; alpha holds neither.
        let bindings = rig.beta.bindings();
        assert_eq!(bindings.len(), 1);
        let binding = bindings[0].clone();
        assert_eq!(binding.mode, Mode::App);
        assert_eq!(binding.status, Status::Active);
        assert_eq!(
            binding.label_sets,
            vec![
                vec!["self-hosted".to_string(), "linux".to_string()],
                vec!["gpu".to_string()]
            ]
        );
        assert!(
            rig.scratch
                .path("beta/github/keys")
                .join(format!("{}.pem", binding.id))
                .is_file()
        );
        assert!(rig.alpha.bindings().is_empty());
        assert!(!rig.scratch.path("alpha/github/keys").exists());
        for (path, text) in rig.alpha_files() {
            assert!(
                !text.contains("BEGIN RSA"),
                "{} holds the App's key",
                path.display()
            );
            // Alpha's own OAuth App secret is its own, set by the rig.
            if !path.ends_with("github/oauth-app.secret") {
                assert_no_secret(&path.display().to_string(), &text);
            }
        }
        for (path, text) in files(&rig.scratch.path("beta/github")) {
            if !path.starts_with(rig.scratch.path("beta/github/keys")) {
                assert_no_secret(&path.display().to_string(), &text);
            }
        }

        // Beta's own github_bindings shows it.
        let caller = Caller {
            token_id: "viewer".to_string(),
            scope: Scope::Viewer,
            tools: None,
            limits: Default::default(),
        };
        let listed = handlers(&rig.beta)
            .into_iter()
            .find(|(verb, _)| *verb == "github_bindings")
            .expect("the verb")
            .1;
        let listed = tokio::task::spawn_blocking(move || listed.call(json!({}), &caller))
            .await
            .unwrap()
            .expect("the bindings");
        assert_eq!(listed["bindings"][0]["id"], json!(binding.id));

        // The page says it is connected, and lists beta's connections when the owner asks for them.
        let q = query(&location[location.find('?').unwrap() + 1..]);
        let (_, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(body.contains("Connected"), "{body}");
        assert!(body.contains(&binding.id), "{body}");
        let (status, _, body) =
            read(github_page::connections(&rig.ctx(), &query(&format!("node={NODE}"))).await).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(&format!("name=\"binding\" value=\"{}\"", binding.id)),
            "{body}"
        );
        assert!(body.contains("data-approve"), "{body}");
        assert_no_secret("the page", &body);

        // Disconnect from the page, with the owner's tap, works on beta.
        let form = rig
            .approve(&format!("node={NODE}&binding={}", binding.id))
            .await;
        let (status, location, body) = read(github_page::disconnect(&rig.ctx(), &form).await).await;
        assert_eq!(status, 303, "{body}");
        assert_eq!(
            location.as_deref(),
            Some(format!("{}?node={NODE}", github_page::PAGE_PATH).as_str())
        );
        assert_eq!(rig.beta.bindings()[0].status, Status::Revoked);
        assert!(
            !rig.scratch
                .path("beta/github/keys")
                .join(format!("{}.pem", binding.id))
                .exists()
        );
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 321, finding 1: the relay token is a file on the page's node, and no token, it or any other, begins,
/// finishes, installs or disconnects a connection on beta without the owner's passkey assertion, which beta checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_linked_node_takes_no_step_for_any_token_without_the_owners_approval() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let binding = connect_beta(&rig).await;
        let conversions = rig.conversions();
        let refused = |answer: Result<Value, RelayError>, code: &str, what: &str| {
            let error = answer.expect_err(what);
            assert_eq!(refusal_code(&error), code, "{what}: {error:?}");
        };

        // begin: no approval, a made-up one, and the owner's own passkey answer sent as if it were the grant.
        let begin = json!({ "step": "begin", "origin": ORIGIN, "org": "acme", "purpose": "runners", "labelSets": [["linux"]] });
        refused(rig.direct("relay", LINK_VERB, begin.clone()).await, "NotApproved", "begin, no approval");
        let mut forged = begin.clone();
        forged["approval"] = json!("0123456789abcdef");
        refused(rig.direct("relay", LINK_VERB, forged).await, "NotApproved", "begin, a made-up grant");
        let mut signed = begin.clone();
        signed["approval"] = json!(passkey_approval(
            "{}",
            &rig.owner.assert(&Assert::over(NAMESPACE, b"{}"))
        ));
        refused(rig.direct("relay", LINK_VERB, signed).await, "NotApproved", "begin, a passkey answer as the grant");

        // disconnect: the connection stays.
        let disconnect = json!({ "step": "disconnect", "origin": ORIGIN, "binding": binding.id });
        refused(rig.direct("relay", LINK_VERB, disconnect.clone()).await, "NotApproved", "disconnect");
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);

        // created and installed for a flow no approved begin made: one beta's own page began.
        let own = rig
            .beta
            .app_begin("acme", &[vec!["linux".to_string()]], ORIGIN, "beta-page")
            .expect("beta's own flow");
        let bound = |step: &str| {
            json!({ "step": step, "origin": ORIGIN, "state": own.state, "plan": "act-0123456789ab", "org": "acme",
                    "purpose": "runners", "code": MANIFEST_CODE, "installationId": INSTALLATION })
        };
        refused(rig.direct("relay", LINK_VERB, bound("created")).await, "UnknownFlow", "created, not approved");
        refused(rig.direct("relay", LINK_VERB, bound("installed")).await, "UnknownFlow", "installed, not approved");

        // An approved flow, named with another proposal, organisation or kind, or from a page beta does not trust.
        let form = rig.approve(BEGIN_FORM).await;
        let plan = form["plan"].clone();
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_eq!(status, 200, "{body}");
        let action = body
            .split("action=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the form's action")
            .replace("&amp;", "&");
        let state = state_of(&action);
        let step = |change: &dyn Fn(&mut Value)| {
            let mut args = json!({ "step": "created", "origin": ORIGIN, "state": state, "plan": plan, "org": "acme",
                                   "purpose": "runners", "code": MANIFEST_CODE });
            change(&mut args);
            args
        };
        refused(rig.direct("relay", LINK_VERB, step(&|a| a["plan"] = json!("act-0123456789ab"))).await, "UnknownFlow", "another proposal");
        refused(rig.direct("relay", LINK_VERB, step(&|a| a["org"] = json!("other"))).await, "UnknownFlow", "another organisation");
        refused(rig.direct("relay", LINK_VERB, step(&|a| a["purpose"] = json!("repositories"))).await, "UnknownFlow", "another kind");
        refused(rig.direct("relay", LINK_VERB, step(&|a| a["origin"] = json!("https://forged.example.com"))).await, "NotApprovalHost", "a forged origin");
        assert_eq!(rig.conversions(), conversions, "GitHub was asked for nothing");

        // A caller that is not the relay token: an agent's doctor token is not offered the verb and is refused; an
        // operator token reaches it and takes no step without an approval either.
        let agent = rig.as_token("agent");
        let relay = rig.as_token("relay");
        let offered = tokio::task::spawn_blocking(move || {
            (linked::admits(&agent, LINK_VERB), linked::admits(&relay, LINK_VERB))
        })
        .await
        .unwrap();
        assert_eq!(offered, (Some(false), Some(true)));
        refused(rig.direct("agent", LINK_VERB, begin.clone()).await, "Token", "an agent's token");
        refused(rig.direct("operator", LINK_VERB, begin).await, "NotApproved", "an operator token, no approval");
        refused(rig.direct("operator", LINK_VERB, disconnect).await, "NotApproved", "an operator disconnect");
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
        assert_eq!(rig.beta.bindings().len(), 1);
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_misdirected_or_untrusted_assertion_takes_nothing() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;

        // One approval, used once.
        let form = rig.approve(BEGIN_FORM).await;
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_eq!(status, 200, "{body}");
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("name=\"manifest\""), "{body}");

        // An assertion over one card, sent for another: neither runs, and the other card still waits.
        let first = rig.approve("node=beta&org=acme&labels=linux").await;
        let second = rig.approve("node=beta&org=other&labels=linux").await;
        let mut crossed = second.clone();
        crossed.insert("approval".to_string(), first["approval"].clone());
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &crossed).await).await;
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("name=\"manifest\""), "{body}");

        // A passkey beta does not trust, on the same page.
        let stranger = SoftPasskey::from_seed(7);
        let form = rig
            .approve_with("node=beta&org=acme&labels=gpu", &stranger)
            .await;
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_ne!(status, 200, "{body}");
        assert!(!body.contains("name=\"manifest\""), "{body}");

        // A form with no approval at all (a browser that ran no script) asks beta nothing.
        let (status, _, body) =
            read(github_page::app_start(&rig.ctx(), &query(BEGIN_FORM)).await).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("passkey"), "{body}");

        assert!(rig.beta.bindings().is_empty());
        assert!(rig.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_or_replayed_state_takes_nothing_more() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;

        // A state nobody gave: refused before GitHub or beta is asked anything.
        let (status, _, body) = created(&rig, "0123456789abcdef0123456789abcdef").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(rig.conversions(), 0);
        assert!(rig.beta.bindings().is_empty() && rig.alpha.bindings().is_empty());
        assert_no_secret("the refusal", &body);

        // A replayed return of a linked flow: the same install screen, no second App, no second connection.
        let state = begin_on_beta(&rig).await;
        let first = created(&rig, &state).await;
        assert_eq!(first.0, 303, "{}", first.2);
        let again = created(&rig, &state).await;
        assert_eq!(again.0, 303, "{}", again.2);
        assert_eq!(again.1, first.1);
        assert_eq!(rig.conversions(), 1);
        assert_eq!(rig.beta.bindings().len(), 1);

        // A page beta does not trust is told so before any card is made.
        let other = Rig::new(&["https://other.example.com"], &RELAY_TOOLS).await;
        let (status, body) = prepare(&other, &query("node=beta&org=acme&labels=linux")).await;
        assert_eq!(status, 502, "{body}");
        assert!(
            body["message"].as_str().unwrap().contains("trusts"),
            "{body}"
        );
        assert_eq!(other.waiting_cards(), 0);
        assert!(other.fake.with(|s| s.seen.is_empty()));
        assert!(other.beta.bindings().is_empty());
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_is_not_linked_older_or_will_not_take_the_token_is_a_plain_error_and_nothing_is_made()
 {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let (status, _, body) =
            read(github_page::view(&rig.ctx(), &query("mode=app&node=gamma&org=acme")).await).await;
        assert_eq!(status, 400, "{body}");
        assert!(
            body.contains(NODE),
            "the refusal names the linked nodes: {body}"
        );
        assert!(
            !body.contains("gamma"),
            "the refusal never echoes the name: {body}"
        );
        let (status, body) = prepare(&rig, &query("node=gamma&org=acme&labels=linux")).await;
        assert_eq!(status, 400);
        assert!(!body.to_string().contains("gamma"));

        // The form's own words, as the typed form's, before beta is asked anything.
        let (status, body) = prepare(&rig, &query("node=beta&org=-acme&labels=linux")).await;
        assert_eq!(status, 400);
        let typed =
            read(github_page::app_start(&rig.ctx(), &query("org=-acme&labels=linux")).await).await;
        assert!(
            typed
                .2
                .contains(&github_page::escape(body["message"].as_str().unwrap())),
            "{body}"
        );
        assert_eq!(rig.waiting_cards(), 0);

        // A relay token whose list does not name github_link: beta does not offer it, and GitHub hears nothing.
        let narrow = Rig::new(
            &[ORIGIN],
            &["action_propose", "plan_list", "action_approve"],
        )
        .await;
        let (status, body) = prepare(&narrow, &query(BEGIN_FORM)).await;
        assert_eq!(status, 502, "{body}");
        let words = body["message"].as_str().unwrap();
        assert!(
            words.contains(NODE) && words.contains("github_link"),
            "{words}"
        );
        assert_eq!(narrow.waiting_cards(), 0);

        // A linked node on an older Arca, which has no github_link: a plain sentence, never "not_found".
        let mut setup = Setup::new(&[ORIGIN], &RELAY_TOOLS);
        setup.older = true;
        let older = Rig::with(setup).await;
        let (status, body) = prepare(&older, &query(BEGIN_FORM)).await;
        assert_eq!(status, 502, "{body}");
        let words = body["message"].as_str().unwrap();
        assert!(
            words.contains("older Arca") && words.contains(NODE),
            "{words}"
        );
        assert!(!words.contains("not_found"), "{words}");
        assert_eq!(older.waiting_cards(), 0);

        assert!(rig.fake.with(|s| s.seen.is_empty()));
        assert!(narrow.fake.with(|s| s.seen.is_empty()));
        assert!(older.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_down_before_the_start_makes_nothing_on_github() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        rig.beta_down();
        // The page opens: it asks beta nothing.
        let q = query(&format!("mode=app&node={NODE}&org=acme&labels=linux"));
        let (status, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert_eq!(status, 200);
        assert!(!body.contains("could not be reached"), "{body}");
        // The button asks beta for its card, and says it could not.
        let (status, body) = prepare(&rig, &query(BEGIN_FORM)).await;
        assert_eq!(status, 502, "{body}");
        let words = body["message"].as_str().unwrap();
        assert!(
            words.contains(NODE) && words.contains("Nothing was made on GitHub"),
            "{words}"
        );
        assert!(rig.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_down_at_githubs_return_is_told_what_github_holds_and_not_that_nothing_is_stored()
 {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let state = begin_on_beta(&rig).await;
        rig.beta_down();

        // The App exists on GitHub; whether beta took it is not known, and the page does not claim it.
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("Not known yet"), "{body}");
        assert!(!body.contains("nothing of it is stored"), "{body}");
        assert!(
            body.contains(&format!("Arca runners {}", &state[..6])),
            "names the App: {body}"
        );
        assert!(
            body.contains("/organizations/acme/settings/apps"),
            "says where to delete it: {body}"
        );
        assert!(body.contains(NODE));
        assert_no_secret("the page", &body);
        assert_eq!(rig.conversions(), 0);
        assert!(rig.beta.bindings().is_empty() && rig.alpha.bindings().is_empty());

        // Back up within the hour, the same return finishes.
        rig.beta_up().await;
        let (status, location, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        assert!(location.unwrap().contains("/installations/new"));
        assert_eq!(rig.beta.bindings()[0].status, Status::AwaitingInstall);

        // Down again after the App was taken: the owner is told what is on GitHub and on beta.
        rig.beta_down();
        rig.fake.with(|s| s.installed = true);
        let (status, _, body) = installed(&rig, &state).await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains(APP_SLUG), "names the App: {body}");
        assert!(
            body.contains(&format!("/apps/{APP_SLUG}")),
            "links to it: {body}"
        );
        assert!(body.contains(NODE));
        assert_no_secret("the page", &body);
        assert_eq!(rig.beta.bindings()[0].status, Status::AwaitingInstall);

        rig.beta_up().await;
        let (status, location, body) = installed(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        assert!(location.unwrap().contains("connected=gh-"));
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
        assert!(rig.alpha.bindings().is_empty());
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 321, finding 2: beta took the App, but its answer came after the page stopped waiting. The page reads
/// what beta holds and goes on from that, rather than saying the node was never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_answer_is_read_back_from_the_linked_node() {
    tokio::time::timeout(BOUND, async {
        let mut setup = Setup::new(&[ORIGIN], &RELAY_TOOLS);
        setup.wait = Some(Duration::from_secs(1));
        setup.late = vec![("created", Duration::from_secs(4))];
        let rig = Rig::with(setup).await;
        let state = begin_on_beta(&rig).await;
        let (status, location, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let install = location.expect("the install screen beta holds");
        assert!(install.starts_with(&format!(
            "{}/web/apps/{APP_SLUG}/installations/new",
            rig.fake.base
        )));
        assert_eq!(rig.conversions(), 1);
        assert_eq!(rig.beta.bindings().len(), 1);
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 321, finding 3: GitHub's later returns carry no state. They reach the linked node whose App it is, and
/// it brings its runner group to the repositories GitHub now names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn githubs_later_returns_without_a_state_reach_the_linked_node() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;

        // An install finished after the hour: the page no longer knows the flow, and the index names beta's App.
        let state = begin_on_beta(&rig).await;
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        rig.fake.with(|s| s.installed = true);
        let (status, location, body) = installed_later(&rig, "install").await;
        assert_eq!(status, 303, "{body}");
        assert!(location.unwrap().starts_with(&format!(
            "{}?node={NODE}&connected=gh-",
            github_page::PAGE_PATH
        )));
        let binding = rig.beta.bindings()[0].clone();
        assert_eq!(binding.status, Status::Active);
        let group = binding.runner_group.clone().expect("the runner group").id;
        assert_eq!(
            binding.installation.as_ref().unwrap().repositories,
            vec!["acme/web".to_string(), "acme/api".to_string()]
        );

        // The owner removes a repository on GitHub: its return reaches beta, and the repository leaves the pool's group.
        rig.fake
            .with(|s| s.repositories.retain(|(id, _)| *id == 11));
        let (status, location, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 303, "{body}");
        assert!(location.unwrap().contains(&format!("node={NODE}")));
        let binding = rig.beta.bindings()[0].clone();
        assert_eq!(
            binding.installation.as_ref().unwrap().repositories,
            vec!["acme/web".to_string()]
        );
        let kept = rig.fake.with(|s| {
            s.group_repositories
                .iter()
                .find(|(id, _)| *id == group)
                .map(|(_, ids)| ids.clone())
        });
        assert_eq!(kept, Some(json!([11])));

        // The index holds no secret. It names beta's App, but not the installation: this page never saw that
        // installation made in a flow it relayed. A page that starts again still finds beta through its App.
        let index = fs::read_to_string(rig.scratch.path("alpha/github-linked-apps.json"))
            .expect("the index file");
        assert!(index.contains(NODE) && index.contains(APP_SLUG), "{index}");
        let written: Value = serde_json::from_str(&index).expect("the index is JSON");
        assert_eq!(written["installations"], json!({}), "{index}");
        assert_no_secret("the index", &index);
        let again = Links::over(vec![rig.as_token("relay")])
            .kept_in(rig.scratch.path("alpha/github-linked-apps.json"));
        let ctx = Ctx {
            links: &again,
            ..rig.ctx()
        };
        let q = query(&format!(
            "installation_id={INSTALLATION}&setup_action=update"
        ));
        let (status, location, body) = read(github_page::installed(&ctx, &q, true).await).await;
        assert_eq!(status, 303, "{body}");
        assert!(location.unwrap().contains(&format!("node={NODE}")));
        assert!(rig.alpha.bindings().is_empty());
    })
    .await
    .expect("the test ends in time");
}

/// Review 2 of PR 321, L1: beta recorded the installation, but its answer came after the page stopped waiting, and the
/// first read of its connections came late too. The reload the page asks for finds the connection in use, and never
/// tells the owner to undo it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_install_answer_then_a_reload_finds_the_connection_in_use() {
    tokio::time::timeout(BOUND, async {
        let mut setup = Setup::new(&[ORIGIN], &RELAY_TOOLS);
        setup.wait = Some(Duration::from_secs(1));
        setup.late = vec![
            ("installed", Duration::from_secs(3)),
            ("bindings", Duration::from_secs(3)),
        ];
        let rig = Rig::with(setup).await;
        let state = begin_on_beta(&rig).await;
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        rig.fake.with(|s| s.installed = true);

        // Neither answer came in time: the page says it does not know yet, and asks for a reload.
        let (status, _, body) = installed(&rig, &state).await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("not known"), "{body}");
        assert!(!body.contains("Not connected"), "{body}");
        // Beta did record it.
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);

        // The reload: beta no longer knows the flow (it was finished), so the page reads beta's connections.
        let (status, location, body) = installed(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let binding = rig.beta.bindings()[0].id.clone();
        assert_eq!(
            location.as_deref(),
            Some(format!("{}?node={NODE}&connected={binding}", github_page::PAGE_PATH).as_str())
        );
        assert!(!body.contains("uninstall"), "{body}");

        // And once more, the flow now ended: a return without a live state, which the owner's signed-in page reads
        // from the index the relayed flow wrote.
        let q = query(&format!(
            "installation_id={INSTALLATION}&setup_action=install&state={state}"
        ));
        let (status, location, _) = read(github_page::installed(&rig.ctx(), &q, true).await).await;
        assert_eq!(status, 303);
        assert!(location.unwrap().contains(&format!("connected={binding}")));
        let index = fs::read_to_string(rig.scratch.path("alpha/github-linked-apps.json"))
            .expect("the index file");
        let written: Value = serde_json::from_str(&index).expect("the index is JSON");
        assert_eq!(
            written["installations"][INSTALLATION.to_string()],
            json!(NODE)
        );
    })
    .await
    .expect("the test ends in time");
}

/// Review 2 of PR 321, L3: an installation the index names for a node that does not recognise it (here a node no longer
/// linked) is tried on the other linked nodes that took an App from this page, and the stale entry goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_return_the_indexed_node_does_not_recognise_reaches_the_other_linked_nodes() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let state = begin_on_beta(&rig).await;
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        rig.fake.with(|s| s.installed = true);

        // A page that starts again, whose index sends the installation to a node that is gone.
        let file = rig.scratch.path("alpha/github-linked-apps.json");
        let mut written: Value =
            serde_json::from_str(&fs::read_to_string(&file).expect("the index file"))
                .expect("JSON");
        written["installations"] = json!({ INSTALLATION.to_string(): "gamma" });
        fs::write(&file, written.to_string()).expect("the index is written");
        let again = Links::over(vec![rig.as_token("relay")]).kept_in(file.clone());
        let ctx = Ctx {
            links: &again,
            ..rig.ctx()
        };
        let q = query(&format!(
            "installation_id={INSTALLATION}&setup_action=install"
        ));
        let (status, location, body) = read(github_page::installed(&ctx, &q, true).await).await;
        assert_eq!(status, 303, "{body}");
        assert!(
            location
                .unwrap()
                .contains(&format!("node={NODE}&connected=gh-"))
        );
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
        let written: Value =
            serde_json::from_str(&fs::read_to_string(&file).expect("the index file"))
                .expect("JSON");
        assert_eq!(written["installations"], json!({}), "{written}");
    })
    .await
    .expect("the test ends in time");
}

/// A linked node that answers every `github_link` step with `answer`, behind its own door on `socket`, reached with the
/// relay token: one that lies about installations it does not hold.
struct Liar(Value);

impl ToolHandler for Liar {
    fn call(&self, _args: Value, _caller: &Caller) -> Result<Value, CallError> {
        Ok(self.0.clone())
    }
}

impl Rig {
    fn steps(&self) -> Vec<String> {
        self.steps.lock().expect("the steps").clone()
    }

    /// Starts a lying linked node named `name`, which answers `answer`, and answers how to reach it.
    async fn liar(&self, name: &str, answer: Value) -> LinkedHost {
        let tokens = TokenStore::load(&self.scratch.path("beta-tokens")).expect("the token store");
        let router = Server::new(arca_api::verbs(), tokens)
            .handle(LINK_VERB, Arc::new(Liar(answer)))
            .router();
        let socket = self.scratch.path(&format!("{name}.sock"));
        let path = socket.clone();
        tokio::spawn(async move {
            let _ = serve_unix(router, &path).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the liar's door started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        LinkedHost {
            name: name.to_string(),
            socket,
            token_file: self.scratch.path("relay-to-beta.secret"),
            incus_socket: None,
            ceph: None,
        }
    }
}

/// Review 1 of PR 322, P1 and N2: GitHub's return address is public. A return with no live state, sent by anyone
/// without the owner's signed-in page session, asks no node and GitHub nothing, changes nothing, and answers the same
/// whether the installation exists or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stateless_return_without_the_owners_session_does_nothing_and_reveals_nothing() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let binding = connect_beta(&rig).await;
        let file = rig.scratch.path("alpha/github-linked-apps.json");
        let index = fs::read_to_string(&file).expect("the index file");
        assert!(index.contains(&INSTALLATION.to_string()), "{index}");
        let github = rig.fake.with(|s| s.seen.len());
        let steps = rig.steps().len();

        let forged = |installation: u64, extra: &str| {
            query(&format!(
                "installation_id={installation}&setup_action=update{extra}"
            ))
        };
        let mut answers = Vec::new();
        for q in [
            forged(INSTALLATION, ""),
            forged(999_999, ""),
            forged(INSTALLATION, "&state=0123456789abcdef0123456789abcdef"),
            forged(INSTALLATION, "&setup_action=install"),
        ] {
            answers.push(read(github_page::installed(&rig.ctx(), &q, false).await).await);
        }
        for (status, location, body) in &answers {
            assert_eq!(*status, 401, "{body}");
            assert_eq!(*location, None);
            assert!(
                !body.contains(NODE) && !body.contains(&binding.id),
                "{body}"
            );
            assert!(body.contains("Sign in"), "{body}");
        }
        assert!(answers.iter().all(|answer| *answer == answers[0]));

        assert_eq!(
            rig.fake.with(|s| s.seen.len()),
            github,
            "GitHub was asked nothing"
        );
        assert_eq!(rig.steps().len(), steps, "beta was asked nothing");
        assert_eq!(fs::read_to_string(&file).expect("the index file"), index);
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 322, P1: with the owner's session, a state-less return of one installation is read at most once a
/// minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stateless_return_with_the_owners_session_is_read_once_a_minute() {
    tokio::time::timeout(BOUND, async {
        let mut setup = Setup::new(&[ORIGIN], &RELAY_TOOLS);
        setup.throttle = true;
        let rig = Rig::with(setup).await;
        let binding = connect_beta(&rig).await;
        let github = rig.fake.with(|s| s.seen.len());
        let refreshes = |rig: &Rig| rig.steps().iter().filter(|step| *step == "refresh").count();

        let (status, location, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 303, "{body}");
        assert!(
            location
                .unwrap()
                .contains(&format!("node={NODE}&connected={}", binding.id))
        );
        assert_eq!(refreshes(&rig), 1);
        let read_once = rig.fake.with(|s| s.seen.len());
        assert!(read_once > github, "beta read the installation from GitHub");

        // Again within the minute: nothing is asked, and the owner is told when it can be read again.
        let (status, location, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 429, "{body}");
        assert_eq!(location, None);
        assert!(body.contains("minute"), "{body}");
        assert_eq!(refreshes(&rig), 1);
        assert_eq!(rig.fake.with(|s| s.seen.len()), read_once);
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 322, N1: a return of one flow that names another connection's installation is not shown as connected
/// when the flow's own connection is not the one in use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flows_return_is_connected_only_to_its_own_connection() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let first = connect_beta(&rig).await;

        // A second App on beta, made but not installed: GitHub cannot read the installation the return names for it.
        rig.fake.with(|s| {
            s.app_made = false;
            s.installed = false;
        });
        let state = begin_on_beta(&rig).await;
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let (status, location, body) = installed(&rig, &state).await;
        assert_ne!(status, 303, "{body}");
        assert_eq!(location, None);
        assert!(!body.contains(&first.id), "{body}");
        assert!(body.contains("Not connected"), "{body}");
        let bindings = rig.beta.bindings();
        assert_eq!(bindings.len(), 2);
        assert_eq!(
            bindings.iter().find(|b| b.id == first.id).unwrap().status,
            Status::Active
        );
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 322, R1: a linked node's OK for an installation the index does not name counts only when it names an
/// App this page relayed for that node; a node that claims another node's App does not swallow the return.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_that_claims_another_nodes_app_does_not_swallow_its_return() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let state = begin_on_beta(&rig).await;
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        rig.fake.with(|s| s.installed = true);

        // The liar took an App of its own from this page, comes first, and claims beta's App for every installation.
        let liar = rig
            .liar(
                "liar",
                json!({ "binding": "gh-0123456789abcdef", "app": { "id": 7, "slug": APP_SLUG } }),
            )
            .await;
        let file = rig.scratch.path("alpha/github-linked-apps.json");
        fs::write(
            &file,
            json!({ "apps": [["liar", "liar-app"], [NODE, APP_SLUG]], "installations": {} })
                .to_string(),
        )
        .expect("the index is written");
        let links = Links::over(vec![liar, rig.as_token("relay")])
            .kept_in(file.clone())
            .with_reconcile_gap(Duration::ZERO);
        let ctx = Ctx {
            links: &links,
            ..rig.ctx()
        };
        let q = query(&format!(
            "installation_id={INSTALLATION}&setup_action=install"
        ));
        let (status, location, body) = read(github_page::installed(&ctx, &q, true).await).await;
        assert_eq!(status, 303, "{body}");
        let location = location.unwrap();
        assert!(
            location.contains(&format!("node={NODE}&connected=gh-")),
            "{location}"
        );
        assert!(!location.contains("gh-0123456789abcdef"), "{location}");
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 323, L1: a state-less return is acted on only when exactly one node claims the installation. Two
/// claims (here a linked node naming an App this page did relay for it) change nothing, and the owner is told which
/// nodes claim it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_return_two_nodes_claim_is_acted_on_by_neither() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let binding = connect_beta(&rig).await;
        let liar = rig
            .liar(
                "liar",
                json!({ "binding": "gh-0123456789abcdef", "app": { "id": 7, "slug": "liar-app" } }),
            )
            .await;
        let file = rig.scratch.path("alpha/github-linked-apps.json");
        fs::write(
            &file,
            json!({ "apps": [["liar", "liar-app"], [NODE, APP_SLUG]], "installations": {} })
                .to_string(),
        )
        .expect("the index is written");
        let links = Links::over(vec![liar, rig.as_token("relay")])
            .kept_in(file.clone())
            .with_reconcile_gap(Duration::ZERO);
        let ctx = Ctx {
            links: &links,
            ..rig.ctx()
        };
        let github = rig.fake.with(|s| s.seen.len());
        let q = query(&format!(
            "installation_id={INSTALLATION}&setup_action=update"
        ));
        let (status, location, body) = read(github_page::installed(&ctx, &q, true).await).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(location, None);
        assert!(body.contains("liar") && body.contains(NODE), "{body}");
        // Beta was asked whether it holds the installation, and nothing more: no runner group was written.
        let steps = rig.steps();
        assert!(steps.iter().any(|step| step == "claim"), "{steps:?}");
        assert!(!steps.iter().any(|step| step == "refresh"), "{steps:?}");
        let written = rig.fake.with(|s| {
            s.seen[github..]
                .iter()
                .filter(|seen| seen.method != "GET" && !seen.path.ends_with("/access_tokens"))
                .count()
        });
        assert_eq!(written, 0, "nothing was written on GitHub");
        assert_eq!(rig.beta.bindings()[0].id, binding.id);
        assert_eq!(rig.beta.bindings()[0].status, Status::Active);
    })
    .await
    .expect("the test ends in time");
}

/// Review 1 of PR 323, L2: a read that reached no node does not count against the minute: the reload the page asks for
/// once the node answers reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_return_whose_node_did_not_answer_is_read_on_the_next_reload() {
    tokio::time::timeout(BOUND, async {
        let mut setup = Setup::new(&[ORIGIN], &RELAY_TOOLS);
        setup.throttle = true;
        let rig = Rig::with(setup).await;
        let binding = connect_beta(&rig).await;
        rig.beta_down();
        let (status, _, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("Not brought up to date yet"), "{body}");
        rig.beta_up().await;
        let (status, location, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 303, "{body}");
        assert!(
            location
                .unwrap()
                .contains(&format!("connected={}", binding.id))
        );
        // Now it was read: the next one within the minute is not.
        let (status, _, body) = installed_later(&rig, "update").await;
        assert_eq!(status, 429, "{body}");
    })
    .await
    .expect("the test ends in time");
}

struct NoDisplay;

impl SetupCodeDisplay for NoDisplay {
    fn show(&self, _code: &str) -> Result<(), String> {
        Err("No screen to show the code on".to_string())
    }
}

/// One HTTP/1.1 request to the page on its unix socket: the status, the head and the body.
fn http(
    socket: &Path,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut stream = UnixStream::connect(socket).expect("the page answers");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("a read timeout");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: box1.example.com\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("the request");
    stream.write_all(body).expect("the body");
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).expect("the answer");
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a head");
    let head = String::from_utf8_lossy(&answer[..split]).to_string();
    let mut rest = answer[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut decoded = Vec::new();
        loop {
            let line = rest
                .windows(2)
                .position(|w| w == b"\r\n")
                .expect("a chunk size");
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line]).trim(), 16)
                .expect("a hex size");
            if size == 0 {
                break;
            }
            decoded.extend_from_slice(&rest[line + 2..line + 2 + size]);
            rest = rest[line + 2 + size + 2..].to_vec();
        }
        rest = decoded;
    }
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status");
    (status, head, rest)
}

fn head_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// Review 1 of PR 323, L3: GitHub's return address through the page's own router. Without a session it serves the
/// GitHub page's passkey sign-in, the same for any installation, and asks nobody anything; the sign-in POST on the
/// same address takes only the page's own origin; after it, the same address reads the return once, then not again
/// within the minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_return_address_signs_in_first_through_the_router_and_goes_on_after() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &RELAY_TOOLS).await;
        let binding = connect_beta(&rig).await;

        // Alpha's page as the node serves it, with alpha's kernel trusting the owner's passkey.
        let kernel_scratch = Scratch::new("ghlink-alpha-kernel");
        let passkeys = PasskeyVerifier::new(
            arca_approval::parse_passkeys(format!("{}\n", rig.owner.line(NAMESPACE)).as_bytes()).expect("the owner passkey"),
            FileSignCounts::open(&kernel_scratch.path("alpha-counts.jsonl")).expect("counts"),
        );
        let verifier = ApprovalVerifier::new(
            "alpha.example".to_string(),
            OwnerKeys::new(SshSigVerifier::new(Vec::new()), passkeys),
            FileNonces::open(&kernel_scratch.path("alpha-nonces.jsonl")).expect("nonces"),
        );
        let incus = start_fake(&kernel_scratch);
        let kernel = Arc::new(Mutex::new(
            kernel_on_host(&kernel_scratch, &incus, verifier, now_ms(), None, "alpha.example")
                .expect("alpha's kernel opens"),
        ));
        let page = Page::new(
            ORIGIN.to_string(),
            kernel,
            clock(),
            rig.scratch.path("alpha"),
            Arc::new(NoDisplay),
        )
        .with_github(rig.alpha.clone())
        .with_linked_hosts(vec![rig.as_token("relay")]);
        let router = page.router(false);
        let socket = rig.scratch.path("alpha-page.sock");
        let served = socket.clone();
        tokio::spawn(async move {
            let _ = serve_unix(router, &served).await;
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "alpha's page started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let call = |method: &'static str, path: String, headers: Vec<(&'static str, String)>, body: Vec<u8>| {
            let socket = socket.clone();
            tokio::task::spawn_blocking(move || {
                let headers: Vec<(&str, &str)> =
                    headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
                http(&socket, method, &path, &headers, &body)
            })
        };
        let path = github_page::INSTALLED_PATH;
        let github = rig.fake.with(|s| s.seen.len());
        let steps = rig.steps().len();

        // No session: the sign-in page, byte for byte the same whatever the address names.
        let mut bodies = Vec::new();
        for q in [
            format!("installation_id={INSTALLATION}&setup_action=update"),
            "installation_id=999999&setup_action=update".to_string(),
            format!("installation_id={INSTALLATION}&setup_action=install&state=0123456789abcdef0123456789abcdef"),
        ] {
            let (status, head, body) = call("GET", format!("{path}?{q}"), Vec::new(), Vec::new())
                .await
                .unwrap();
            assert_eq!(status, 200, "{head}");
            let text = String::from_utf8_lossy(&body).to_string();
            assert!(text.contains("<h1>GitHub runners</h1>"), "{text}");
            assert!(!text.contains(&binding.id) && !text.contains("node=beta"), "{text}");
            bodies.push(body);
        }
        assert!(bodies.iter().all(|body| *body == bodies[0]));
        assert_eq!(rig.fake.with(|s| s.seen.len()), github, "GitHub was asked nothing");
        assert_eq!(rig.steps().len(), steps, "beta was asked nothing");

        // The sign-in POST on the return address: only from the page's own origin.
        let json_type = ("Content-Type", "application/json".to_string());
        let (status, _, _) = call("POST", path.to_string(), vec![json_type.clone()], b"{}".to_vec())
            .await
            .unwrap();
        assert_eq!(status, 403, "no Origin");
        let (status, _, _) = call(
            "POST",
            path.to_string(),
            vec![json_type.clone(), ("Origin", "https://other.example.com".to_string())],
            b"{}".to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(status, 403, "another origin");
        let own = ("Origin", ORIGIN.to_string());
        let (status, _, body) = call(
            "POST",
            path.to_string(),
            vec![json_type.clone(), own.clone()],
            b"{}".to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(status, 200);
        let statement = serde_json::from_slice::<Value>(&body).expect("JSON")["statement"]
            .as_str()
            .expect("a statement")
            .to_string();
        let forged = json!({ "statement": "x", "proof": {} }).to_string().into_bytes();
        let (status, _, _) = call("POST", path.to_string(), vec![json_type.clone(), own.clone()], forged)
            .await
            .unwrap();
        assert_eq!(status, 403, "a forged statement");
        let signed = json!({
            "statement": statement,
            "proof": rig.owner.proof_value(&Assert::over(NAMESPACE, statement.as_bytes())),
        })
        .to_string()
        .into_bytes();
        let (status, head, body) = call("POST", path.to_string(), vec![json_type, own], signed)
            .await
            .unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let cookie = head_value(&head, "set-cookie")
            .expect("a session cookie")
            .split(';')
            .next()
            .expect("its pair")
            .to_string();
        assert_eq!(rig.steps().len(), steps, "signing in asked beta nothing");

        // The reload, now with the session: the return is read once.
        let reload = format!("{path}?installation_id={INSTALLATION}&setup_action=update");
        let (status, head, _) = call("GET", reload.clone(), vec![("Cookie", cookie.clone())], Vec::new())
            .await
            .unwrap();
        assert_eq!(status, 303, "{head}");
        assert_eq!(
            head_value(&head, "location"),
            Some(format!("{}?node={NODE}&connected={}", github_page::PAGE_PATH, binding.id))
        );
        assert_eq!(rig.steps().iter().filter(|step| *step == "refresh").count(), 1);
        let (status, head, _) = call("GET", reload, vec![("Cookie", cookie)], Vec::new())
            .await
            .unwrap();
        assert_eq!(status, 429, "{head}");
        assert_eq!(rig.steps().iter().filter(|step| *step == "refresh").count(), 1);
    })
    .await
    .expect("the test ends in time");
}
