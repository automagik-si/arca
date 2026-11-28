//! The owner's GitHub page with a prepared link, and a linked node connected from it (issue 320), against the fake GitHub
//! (`tests/github/fake.rs`).
//!
//! Alpha serves the page; beta is a linked node with no page of its own: an MCP door on its own unix socket, with the
//! relay token alpha holds (doctor scope, its tool list naming `github_link`). The page drives beta through
//! `crate::linked` exactly as it does in production. Beta converts GitHub's code itself, so the App's key is made into
//! a file on beta alone: no file, answer or page of alpha holds it.

#[path = "github/fake.rs"]
mod fake;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arca::config::LinkedHost;
use arca::github_api::Secret;
use arca::github_handlers::{LINK_VERB, handlers, link_handler};
use arca::github_page::{self, Ctx, Links};
use arca::github_signer::{Mode, Signer, Status};
use arca::handlers::Clock;
use arca_api::Scope;
use arca_mcp::{Caller, Server, TokenStore, mint_with_tools, serve_unix};
use axum::response::Response;
use serde_json::{Value, json};

use fake::{APP_SLUG, CLIENT_ID, CLIENT_SECRET, Fake, INSTALLATION, MANIFEST_CODE, SECRETS};

/// The page's origin: GitHub sends the browser back here, for this node and for a linked one.
const ORIGIN: &str = "https://page.example.com";
/// The page host's own name.
const HERE: &str = "alpha";
/// The linked node's name, as `approval.linkedHosts` lists it.
const NODE: &str = "beta";
/// The passkey session the page acts for.
const CALLER: &str = "page:passkey-1";
/// The longest any one test may take.
const BOUND: Duration = Duration::from_secs(60);

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "arca-ghlink-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("a scratch directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
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

fn write_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("a test file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("its mode");
}

/// Alpha, the page host, and beta, the linked node, on one fake GitHub.
struct Rig {
    scratch: Scratch,
    fake: Fake,
    alpha: Signer,
    beta: Signer,
    links: Links,
    beta_socket: PathBuf,
    beta_router: axum::Router,
}

impl Rig {
    /// Beta trusts the page passkeys of `trusted` origins, and alpha's relay token on beta names `tools`.
    async fn new(trusted: &[&str], tools: &[&str]) -> Self {
        let scratch = Scratch::new();
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

        // Beta's MCP door: the relay token alpha holds, doctor scope, limited to `tools`.
        let tokens_dir = scratch.path("beta-tokens");
        fs::create_dir(&tokens_dir).expect("the token directory");
        fs::set_permissions(&tokens_dir, fs::Permissions::from_mode(0o700)).expect("0700");
        let tools: Vec<String> = tools.iter().map(|tool| tool.to_string()).collect();
        let (secret, hash) = mint_with_tools(Scope::Doctor, Some(&tools));
        write_mode(&tokens_dir.join("relay.hash"), &hash, 0o400);
        let token_file = scratch.path("alpha-relay-to-beta.secret");
        write_mode(&token_file, &format!("{secret}\n"), 0o600);
        let tokens = TokenStore::load(&tokens_dir).expect("the token store loads");
        let trusted: Vec<String> = trusted.iter().map(|origin| origin.to_string()).collect();
        let beta_router = Server::new(arca_api::verbs(), tokens)
            .handle(LINK_VERB, link_handler(&beta, trusted))
            .router();
        let beta_socket = scratch.path("beta.sock");
        let links = Links::over(vec![LinkedHost {
            name: NODE.to_string(),
            socket: beta_socket.clone(),
            token_file,
            incus_socket: None,
            ceph: None,
        }]);
        let rig = Self {
            scratch,
            fake,
            alpha,
            beta,
            links,
            beta_socket,
            beta_router,
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
        }
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

    /// Every file of alpha's, as text.
    fn alpha_files(&self) -> Vec<(PathBuf, String)> {
        files(&self.scratch.path("alpha"))
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

/// Begins an App flow on beta from the page: the manifest's state.
async fn begin_on_beta(rig: &Rig) -> String {
    let form = query(&format!(
        "node={NODE}&org=acme&labels=self-hosted%2C+linux%0D%0Agpu"
    ));
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
    read(github_page::installed(&rig.ctx(), &q).await).await
}

fn submit_buttons(body: &str) -> usize {
    body.matches("type=\"submit\"").count()
}

// ---- The prepared link, this node ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepared_app_link_fills_the_form_and_posts_exactly_as_a_typed_one() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;
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
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;

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
async fn a_linked_node_is_connected_from_this_page_and_holds_the_key_alone() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;

        // The prepared link for the linked node names it and carries it in the form.
        let q = query(&format!("mode=app&node={NODE}&org=acme&labels=linux%3Bgpu"));
        let (status, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(&format!("linked node {NODE}")), "{body}");
        assert!(
            body.contains(&format!("name=\"node\" value=\"{NODE}\"")),
            "{body}"
        );
        assert_eq!(submit_buttons(&body), 1);

        let state = begin_on_beta(&rig).await;
        let (status, location, body) = created(&rig, &state).await;
        assert_eq!(status, 303, "{body}");
        let install = location.expect("the install screen");
        assert!(install.contains(&format!("/apps/{APP_SLUG}/installations/new")));
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

        // The page shows the linked node's connection, by the node's name, and its connected words.
        let q = query(&location[location.find('?').unwrap() + 1..]);
        let (_, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert!(body.contains("Connected"), "{body}");
        assert!(
            body.contains(&format!("name=\"binding\" value=\"{}\"", binding.id)),
            "{body}"
        );
        assert_no_secret("the page", &body);

        // Disconnect from the page works on beta.
        let form = query(&format!("node={NODE}&binding={}", binding.id));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_or_replayed_state_takes_nothing_more() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;

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

        // A state beta issued, sent by a page beta does not trust, takes nothing.
        let other = Rig::new(&["https://other.example.com"], &[LINK_VERB]).await;
        let form = query(&format!("node={NODE}&org=acme&labels=linux"));
        let (status, _, body) = read(github_page::app_start(&other.ctx(), &form).await).await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("approval"), "{body}");
        assert!(other.fake.with(|s| s.seen.is_empty()));
        assert!(other.beta.bindings().is_empty());
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_is_not_linked_or_will_not_take_the_token_is_a_plain_error_and_nothing_is_made()
{
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;
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
        let (status, _, _) = read(
            github_page::app_start(&rig.ctx(), &query("node=gamma&org=acme&labels=linux")).await,
        )
        .await;
        assert_eq!(status, 400);

        // A relay token whose list does not name github_link: beta refuses it, and GitHub hears nothing.
        let narrow = Rig::new(&[ORIGIN], &["github_bindings"]).await;
        let (status, _, body) = read(
            github_page::app_start(
                &narrow.ctx(),
                &query(&format!("node={NODE}&org=acme&labels=linux")),
            )
            .await,
        )
        .await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains(NODE));
        assert!(rig.fake.with(|s| s.seen.is_empty()));
        assert!(narrow.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_down_before_the_start_makes_nothing_on_github() {
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;
        rig.beta_down();
        let q = query(&format!("mode=app&node={NODE}&org=acme&labels=linux"));
        let (status, _, body) = read(github_page::view(&rig.ctx(), &q).await).await;
        assert_eq!(status, 200);
        assert!(body.contains("could not be reached"), "{body}");
        let form = query(&format!("node={NODE}&org=acme&labels=linux"));
        let (status, _, body) = read(github_page::app_start(&rig.ctx(), &form).await).await;
        assert_eq!(status, 502, "{body}");
        assert!(
            body.contains(NODE) && body.contains("Nothing was made on GitHub"),
            "{body}"
        );
        assert!(!body.contains("manifest"));
        assert!(rig.fake.with(|s| s.seen.is_empty()));
    })
    .await
    .expect("the test ends in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_node_down_at_githubs_return_is_told_what_github_holds_and_nothing_is_half_stored()
{
    tokio::time::timeout(BOUND, async {
        let rig = Rig::new(&[ORIGIN], &[LINK_VERB]).await;
        let state = begin_on_beta(&rig).await;
        rig.beta_down();

        // The App exists on GitHub; nobody took its key.
        let (status, _, body) = created(&rig, &state).await;
        assert_eq!(status, 502, "{body}");
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
        assert!(!rig.scratch.path("beta/github/keys").exists());

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
