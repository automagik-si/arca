//! The owner's GitHub page, `/approve/github`, and the push intake, `/github/intake/{binding}`.
//!
//! The page is behind the passkey session of the canvas (the same 12-hour cookie): a person who is not signed in gets the
//! passkey sign-in and nothing else. Signed in, they see their connections and every step of each, and connect an
//! organisation in one of the three ways `crate::github_signer` describes. GitHub's two returns of the App's manifest
//! flow (`/approve/github/created` and `/approve/github/installed`) carry no cookie (GitHub's redirect is a cross-site
//! navigation and the cookie is `SameSite=Strict`): they are bound instead to the flow's random `state`, which only the
//! signed-in browser was given and which lives an hour.
//!
//! The intake is a separate router with no page rule: a server posts there with the binding's intake token as its
//! bearer, and a request that carries an `Origin` (a browser) is refused.
//!
//! **A prepared link.** `/approve/github?mode=app&org=<org>&labels=<set>[;<set>…]` opens the page with that mode's form
//! filled and one button: `mode=repositories` (with `org`, or `repository=<owner>/<name>`), `mode=push` (`org`, `labels`)
//! and `mode=oauth` (`org`, `group`, `labels`, `runners`) too. The values stay editable and are checked when the form is
//! posted, exactly as typed ones are; the passkey session is the same.
//!
//! **A linked node.** `node=<name>` (one of `approval.linkedHosts`, as `arca_status` lists them) connects that node from
//! this page. Opening the page asks the node nothing: the form is drawn from the link and the list of linked nodes. The
//! form's one button is the owner's approval: the page has the node record a `github_link` action proposal for exactly
//! what the form says (the organisation, the kind of App, the label sets and this page, which GitHub returns to), the
//! owner's passkey signs its statement, and the node checks that assertion itself, against the owner keys it trusts for
//! approvals, and spends its nonce before it begins the flow (`crate::github_handlers`). A disconnect is approved the
//! same way. GitHub's returns are relayed for the flow that approval made, with the proposal's id, and need no second
//! tap. The linked node converts GitHub's code into the App and its key itself, so the key never reaches this node: what
//! passes through here is GitHub's one-hour code, in one relayed call's body, never logged, journaled or shown.
//!
//! This node remembers, for an hour and in memory only, which node each flow's `state` belongs to, its proposal and the
//! App's name, so a node that stops answering can be named with what GitHub holds and where to delete it. It also keeps a
//! small index with no secret in it (each linked node's Apps and the installations GitHub named), so GitHub's later
//! returns, which carry no `state` (repositories changed, or an install finished after the hour), reach the node whose App
//! it is.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::LinkedHost;
use crate::github_handlers::LINK_VERB;
use crate::github_oauth::{FlowState, OauthRequest, StartError};
use crate::github_signer::{
    Binding, GroupAccess, Mode, Signer, Status, check_account, check_labels,
};
use crate::linked::RelayError;

/// The page's own path, and its forms and returns.
pub const PAGE_PATH: &str = "/approve/github";
pub const DEVICE_PATH: &str = "/approve/github/device";
pub const APP_PATH: &str = "/approve/github/app";
pub const PUSH_PATH: &str = "/approve/github/push";
pub const DISCONNECT_PATH: &str = "/approve/github/disconnect";
pub const CREATED_PATH: &str = "/approve/github/created";
pub const INSTALLED_PATH: &str = "/approve/github/installed";
/// A linked node's form asks here for the card its button approves: the node records the proposal, and this page answers
/// the statement the owner's passkey signs.
pub const PREPARE_PATH: &str = "/approve/github/prepare";
/// A linked node's connections, read when the owner asks for them.
pub const CONNECTIONS_PATH: &str = "/approve/github/connections";
/// The push intake: `POST /github/intake/{binding}`.
pub const INTAKE_PATH: &str = "/github/intake/{binding}";

/// The page template. Its one `<style>` and its one `<script>` are hashed into the page origin's CSP.
pub const TEMPLATE: &str = include_str!("github_page.html");

/// What a page handler needs.
pub struct Ctx<'a> {
    pub signer: &'a Signer,
    /// The page origin, which GitHub sends the browser back to.
    pub origin: &'a str,
    /// Who acts, for the audit log.
    pub caller: &'a str,
    /// This node's own name, as the page names it: its approval host.
    pub here: &'a str,
    /// The linked nodes the page connects, and the flows it began on them.
    pub links: &'a Links,
    /// The owner passkeys this page trusts, by credential id: the passkey prompt offers them.
    pub credentials: &'a [String],
}

/// How long the page remembers which linked node a manifest flow belongs to: GitHub's code lives as long.
const LINKED_FLOW_TTL_MS: u64 = 60 * 60 * 1000;

/// The relay to the linked nodes.
pub trait Relay: Send + Sync {
    /// The linked nodes' names.
    fn names(&self) -> Vec<String>;
    /// Calls `verb` with `args` on the linked node `node`, waiting up to `wait`. Blocking.
    fn call(
        &self,
        node: &str,
        verb: &str,
        args: Value,
        wait: Duration,
    ) -> Result<Value, RelayError>;
}

/// The relay over each linked host's own socket and token, as `crate::linked` makes every call.
struct HostsRelay(Vec<LinkedHost>);

impl Relay for HostsRelay {
    fn names(&self) -> Vec<String> {
        self.0.iter().map(|host| host.name.clone()).collect()
    }

    fn call(
        &self,
        node: &str,
        verb: &str,
        args: Value,
        wait: Duration,
    ) -> Result<Value, RelayError> {
        let host = self
            .0
            .iter()
            .find(|host| host.name == node)
            .ok_or(RelayError::Unreachable)?;
        crate::linked::call_within(host, verb, args, wait)
    }
}

/// A manifest flow this page began on a linked node: what the owner is told if the node stops answering.
#[derive(Clone, Debug)]
struct LinkedFlow {
    node: String,
    /// The approved proposal whose begin made the flow; the node takes the later steps for it alone.
    plan: String,
    account: String,
    repositories: bool,
    app_name: String,
    created_at: u64,
    /// The App's name on GitHub and its settings page, once the node took it.
    app: Option<(String, String)>,
    /// The connection the node made for the flow's App, once it took it.
    binding: Option<String>,
}

impl LinkedFlow {
    /// The fields every later step of this flow names: its proposal, organisation and kind.
    fn bound(&self, origin: &str, state: &str) -> Value {
        json!({
            "origin": origin,
            "state": state,
            "plan": self.plan,
            "org": self.account,
            "purpose": if self.repositories { "repositories" } else { "runners" },
        })
    }
}

/// Which linked node holds which App and installation, so GitHub's later returns, which carry no `state`, reach it. No
/// secret is in it: node names, App names and GitHub's installation numbers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Index {
    /// `(node, App slug)`, one per App a linked node took.
    #[serde(default)]
    apps: Vec<(String, String)>,
    /// The node each installation GitHub named belongs to.
    #[serde(default)]
    installations: BTreeMap<u64, String>,
}

/// The linked nodes this page connects, the manifest flows it began on them (by `state`, in memory), and the index of
/// their Apps (in a file, when one is given).
pub struct Links {
    relay: Arc<dyn Relay>,
    flows: Mutex<HashMap<String, LinkedFlow>>,
    /// How long a relayed `github_link` step waits: past GitHub's own limits.
    wait: Duration,
    index: Mutex<Index>,
    index_file: Option<PathBuf>,
    /// When each installation's state-less return was last read, in epoch milliseconds.
    reconciled: Mutex<HashMap<u64, u64>>,
    /// How long after one state-less return of an installation the next is read.
    reconcile_gap: Duration,
    /// How long all the nodes asked about a state-less return have to answer, together.
    claim_bound: Duration,
    /// The installations whose state-less return is being read now.
    reading: Mutex<std::collections::HashSet<u64>>,
}

/// A state-less return of one installation being read; dropping it ends the mark.
struct Reading<'a> {
    links: &'a Links,
    installation: u64,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.links
            .reading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.installation);
    }
}

/// How long all the nodes asked whether they hold an installation have, together.
pub const CLAIM_BOUND: Duration = Duration::from_secs(30);

/// How often a state-less return of one installation is read: each read costs its node several GitHub calls.
pub const RECONCILE_GAP: Duration = Duration::from_secs(60);

impl Links {
    /// No linked node.
    pub fn none() -> Self {
        Self::over(Vec::new())
    }

    /// The linked hosts of `approval.linkedHosts`, reached as `crate::linked` reaches them.
    pub fn over(hosts: Vec<LinkedHost>) -> Self {
        Self::new(Arc::new(HostsRelay(hosts)))
    }

    pub fn new(relay: Arc<dyn Relay>) -> Self {
        Self {
            relay,
            flows: Mutex::new(HashMap::new()),
            wait: crate::linked::timeout_for(LINK_VERB),
            index: Mutex::new(Index::default()),
            index_file: None,
            reconciled: Mutex::new(HashMap::new()),
            reconcile_gap: RECONCILE_GAP,
            claim_bound: CLAIM_BOUND,
            reading: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// The same, giving the nodes asked about a state-less return `bound` to answer, together.
    pub fn with_claim_bound(mut self, bound: Duration) -> Self {
        self.claim_bound = bound;
        self
    }

    /// The same, reading a state-less return of one installation at most once per `gap`.
    pub fn with_reconcile_gap(mut self, gap: Duration) -> Self {
        self.reconcile_gap = gap;
        self
    }

    /// Whether a state-less return of `installation` was read less than the gap ago.
    fn read_lately(&self, installation: u64, now: u64) -> bool {
        let gap = self.reconcile_gap.as_millis() as u64;
        let mut reconciled = self
            .reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reconciled.retain(|_, at| now.saturating_sub(*at) < gap);
        reconciled.contains_key(&installation)
    }

    /// Marks a state-less return of `installation` as being read; `None` when one already is. The mark goes with the
    /// guard.
    fn start_reading(&self, installation: u64) -> Option<Reading<'_>> {
        let fresh = self
            .reading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(installation);
        fresh.then_some(Reading {
            links: self,
            installation,
        })
    }

    /// One call to `node`, started now and run on its own: several run at the same time.
    fn start_call(
        &self,
        node: &str,
        args: Value,
        wait: Duration,
    ) -> tokio::task::JoinHandle<Result<Value, RelayError>> {
        let relay = Arc::clone(&self.relay);
        let node = node.to_string();
        tokio::task::spawn_blocking(move || relay.call(&node, LINK_VERB, args, wait))
    }

    /// Notes that a state-less return of `installation` was read: the update was made and answered.
    fn note_read(&self, installation: u64, now: u64) {
        if self.reconcile_gap.is_zero() {
            return;
        }
        self.reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(installation, now);
    }

    /// The same, with each relayed `github_link` step waiting up to `wait`.
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// The same, keeping the index of the linked nodes' Apps in `file` (read now, written on each change).
    pub fn kept_in(mut self, file: PathBuf) -> Self {
        let index = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
            .unwrap_or_default();
        self.index = Mutex::new(index);
        self.index_file = Some(file);
        self
    }

    fn names(&self) -> Vec<String> {
        self.relay.names()
    }

    async fn call(
        &self,
        node: &str,
        verb: &str,
        args: Value,
        wait: Duration,
    ) -> Result<Value, RelayError> {
        let relay = Arc::clone(&self.relay);
        let node = node.to_string();
        let verb = verb.to_string();
        tokio::task::spawn_blocking(move || relay.call(&node, &verb, args, wait))
            .await
            .unwrap_or(Err(RelayError::Unreachable))
    }

    /// One `github_link` step on `node`.
    async fn link(&self, node: &str, args: Value) -> Result<Value, RelayError> {
        self.call(node, LINK_VERB, args, self.wait).await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LinkedFlow>> {
        self.flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, state: &str, flow: LinkedFlow) {
        let mut flows = self.lock();
        let now = flow.created_at;
        flows.retain(|_, kept| now.saturating_sub(kept.created_at) < LINKED_FLOW_TTL_MS);
        flows.insert(state.to_string(), flow);
    }

    fn flow(&self, state: &str, now: u64) -> Option<LinkedFlow> {
        self.lock()
            .get(state)
            .filter(|flow| now.saturating_sub(flow.created_at) < LINKED_FLOW_TTL_MS)
            .cloned()
    }

    fn update(&self, state: &str, change: impl FnOnce(&mut LinkedFlow)) {
        if let Some(flow) = self.lock().get_mut(state) {
            change(flow);
        }
    }

    fn forget(&self, state: &str) {
        self.lock().remove(state);
    }

    /// Changes the index and writes it to its file, when it has one. A write that fails leaves the index in memory.
    fn index_change(&self, change: impl FnOnce(&mut Index)) {
        let mut index = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        change(&mut index);
        if let (Some(file), Ok(text)) = (&self.index_file, serde_json::to_vec_pretty(&*index)) {
            let temporary = file.with_extension("tmp");
            if std::fs::write(&temporary, text).is_err()
                || std::fs::rename(&temporary, file).is_err()
            {
                eprintln!("arca: the index of the linked nodes' GitHub Apps was not written");
            }
        }
    }

    fn index(&self) -> Index {
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn note_app(&self, node: &str, slug: &str) {
        self.index_change(|index| {
            let entry = (node.to_string(), slug.to_string());
            if !index.apps.contains(&entry) {
                index.apps.push(entry);
            }
        });
    }

    /// Notes the node an installation belongs to: only for one GitHub installed in a flow this page relayed.
    fn note_installation(&self, installation: u64, node: &str) {
        self.index_change(|index| {
            index.installations.insert(installation, node.to_string());
        });
    }

    fn forget_installation(&self, installation: u64) {
        self.index_change(|index| {
            index.installations.remove(&installation);
        });
    }
}

/// The node a page request is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Here,
    Linked(String),
}

/// The node a form or link names with `node`: this one when it names none or this node's own name, a linked node by its
/// name, and otherwise a plain error that names the linked nodes (never the name it was given).
fn target_of(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Result<Target, Box<Response>> {
    let node = field(form, "node");
    if node.is_empty() || node == ctx.here {
        return Ok(Target::Here);
    }
    let names = ctx.links.names();
    if names.iter().any(|name| name == node) {
        return Ok(Target::Linked(node.to_string()));
    }
    Err(Box::new(message_page(
        StatusCode::BAD_REQUEST,
        "Not a linked node",
        &not_linked_words(&names),
    )))
}

fn not_linked_words(names: &[String]) -> String {
    if names.is_empty() {
        "That is not a node linked to this one: no node is linked to it.".to_string()
    } else {
        format!(
            "That is not a node linked to this one. The linked nodes are: {}.",
            names.join(", ")
        )
    }
}

/// The hidden `node` field of a form for `target`.
fn node_input(target: &Target) -> String {
    match target {
        Target::Here => String::new(),
        Target::Linked(node) => format!(
            "<input type=\"hidden\" name=\"node\" value=\"{}\">",
            escape(node)
        ),
    }
}

/// What a form whose button is the owner's approval carries for `target`: the node, the empty fields the passkey fills
/// in, and the line the page writes what it is doing in. Nothing for this node's own forms.
fn approve_inputs(target: &Target) -> String {
    match target {
        Target::Here => String::new(),
        Target::Linked(_) => format!(
            "{}<input type=\"hidden\" name=\"plan\" value=\"\"><input type=\"hidden\" name=\"approval\" value=\"\">\
             <p class=\"note\" role=\"status\"></p>",
            node_input(target)
        ),
    }
}

/// The opening tag of a form posted to `action` for `target`: a linked node's is approved with the owner's passkey.
fn form_open(target: &Target, action: &str) -> String {
    match target {
        Target::Here => format!("<form method=\"post\" action=\"{action}\">"),
        Target::Linked(_) => format!("<form method=\"post\" action=\"{action}\" data-approve>"),
    }
}

/// The credential ids the passkey prompt offers, for the page's script.
fn passkey_ids(ctx: &Ctx<'_>) -> String {
    format!(
        "<div id=\"passkeys\" hidden data-ids=\"{}\"></div>",
        escape(&serde_json::to_string(ctx.credentials).unwrap_or_else(|_| "[]".to_string()))
    )
}

/// The page's own address for `target`, with `more` query pairs.
fn page_for(target: &Target, more: &str) -> String {
    match (target, more.is_empty()) {
        (Target::Here, true) => PAGE_PATH.to_string(),
        (Target::Here, false) => format!("{PAGE_PATH}?{more}"),
        (Target::Linked(node), true) => format!("{PAGE_PATH}?node={node}"),
        (Target::Linked(node), false) => format!("{PAGE_PATH}?node={node}&{more}"),
    }
}

/// The tools the relay token this page holds for a linked node names, for the GitHub connection.
const LINK_TOOLS: &str = "github_link, action_propose, plan_list and action_approve";

/// Why a relayed call to `node` did not answer, in words. A node that does not know `github_link` runs an older Arca.
fn relay_words(node: &str, error: &RelayError) -> String {
    match error {
        RelayError::Unreachable => format!("The linked node {node} could not be reached just now."),
        RelayError::Token => format!(
            "The linked node {node} does not take this node's token for GitHub connections: the relay token's tools \
             must name {LINK_TOOLS}."
        ),
        RelayError::Failed(why) if why == "not_found" => older_words(node),
        RelayError::Refused { message, .. } => {
            format!("The linked node {node} refused: {message}.")
        }
        RelayError::Failed(why) => why.clone(),
    }
}

/// What the owner reads when the linked node runs an Arca from before GitHub connections over the link.
fn older_words(node: &str) -> String {
    format!(
        "The linked node {node} runs an older Arca, which cannot be connected to GitHub from this page. Update {node}, \
         then open this link again."
    )
}

/// The status of a relayed step that did not answer: arguments the node found invalid are the form's, the rest the
/// node's.
fn relay_status(error: &RelayError) -> StatusCode {
    match error {
        RelayError::Failed(why) if why != "not_found" => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Whether the call may not have reached the node, or its answer may not have reached this page: the outcome is unknown.
fn not_reached(error: &RelayError) -> bool {
    matches!(error, RelayError::Unreachable | RelayError::Token)
}

/// Whether `url` is an address on GitHub's own web host, as this node's signer names it: the only place the page sends the
/// browser to from a linked node's answer.
fn on_github(ctx: &Ctx<'_>, url: &str) -> bool {
    let web = ctx.signer.endpoints().web.trim_end_matches('/');
    url.strip_prefix(web)
        .is_some_and(|rest| rest.starts_with('/') && !rest.starts_with("//"))
        && !url.contains(['"', '<', '>', '\\', ' '])
}

/// `text` safe inside HTML text and double-quoted attributes.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&#39;")
}

fn html(body: &str, refresh: bool) -> Response {
    let page = TEMPLATE
        .replace(
            "__ARCA_REFRESH__",
            if refresh {
                "<meta http-equiv=\"refresh\" content=\"3\">"
            } else {
                ""
            },
        )
        .replace("__ARCA_BODY__", body);
    let mut response = page.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

fn see_other(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

fn message_page(status: StatusCode, title: &str, text: &str) -> Response {
    let body = format!(
        "<h1>{}</h1><section><p>{}</p><p><a href=\"{PAGE_PATH}\">Back to GitHub runners</a></p></section>",
        escape(title),
        escape(text)
    );
    (status, html(&body, false)).into_response()
}

/// A form or query string, decoded. A key given twice keeps its last value.
pub fn parse_form(text: &str) -> HashMap<String, String> {
    fn decode(part: &str) -> String {
        let bytes = part.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(byte) => {
                            out.push(byte);
                            i += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                byte => out.push(byte),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// One label set from a line: labels split at commas and spaces.
fn label_set(line: &str) -> Vec<String> {
    line.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect()
}

/// Label sets from a text area: one set a line.
fn label_sets(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(label_set)
        .filter(|set| !set.is_empty())
        .collect()
}

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(|value| value.trim()).unwrap_or("")
}

fn status_words(binding: &Binding) -> (&'static str, &'static str) {
    match binding.status {
        Status::Active if binding.open_to_none() => ("bad", "connected, open to no repository"),
        Status::Active if binding.group_access() == Some(GroupAccess::Unchecked) => {
            ("bad", "connected, runner group not checked yet")
        }
        Status::Active => ("ok", "connected"),
        Status::AwaitingInstall => ("bad", "made, not installed yet"),
        Status::Suspended => ("bad", "suspended on GitHub"),
        Status::Revoked => ("bad", "disconnected"),
    }
}

fn mode_words(mode: Mode) -> &'static str {
    match mode {
        Mode::Oauth => "signed in once, runners registered",
        Mode::App => "Arca's App in the organisation",
        Mode::Push => "your service pushes runner configurations",
    }
}

/// The connection rows of `bindings`, each with its Disconnect form for `target`: a linked node's is approved with the
/// owner's passkey.
fn connection_rows(bindings: &[Binding], target: &Target) -> String {
    let mut body = String::new();
    for binding in bindings {
        let (tone, words) = status_words(binding);
        body.push_str(&format!(
            "<div class=\"row\"><strong>{}</strong><span class=\"{tone}\">{words}</span></div><p>{}",
            escape(&binding.account),
            if binding.repository_access { "workspace repository App" } else { mode_words(binding.mode) }
        ));
        if let Some(group) = &binding.runner_group {
            body.push_str(&format!("; runner group \"{}\"", escape(&group.name)));
        }
        body.push_str(&format!(
            "; labels {}",
            escape(
                &binding
                    .label_sets
                    .iter()
                    .map(|set| set.join(", "))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        ));
        if !binding.runners.is_empty() {
            body.push_str(&format!("; {} runner(s)", binding.runners.len()));
        }
        body.push_str(".</p>");
        if let Some(warning) = binding
            .runner_group
            .as_ref()
            .and_then(|group| group.warning.as_ref())
        {
            body.push_str(&format!("<p class=\"bad\">{}</p>", escape(warning)));
        }
        let words = binding.group_access_words();
        // A group's own warning already says what it is open to, unless no job can reach it or it is not checked.
        let warned = binding
            .runner_group
            .as_ref()
            .is_some_and(|group| group.warning.is_some())
            && !binding.group_access_is_news();
        if let Some(words) = words.as_ref().filter(|_| !warned) {
            let tone = if binding.group_access_is_news() {
                " class=\"bad\""
            } else {
                ""
            };
            body.push_str(&format!("<p{tone}>{}</p>", escape(words)));
        }
        if let Some(why) = binding
            .why
            .as_ref()
            .filter(|why| Some(*why) != words.as_ref())
        {
            body.push_str(&format!("<p>{}</p>", escape(why)));
        }
        if binding.status != Status::Revoked {
            body.push_str(&format!(
                "{}{}<input type=\"hidden\" name=\"binding\" value=\"{}\"><button class=\"quiet\" type=\"submit\">{}</button></form>",
                form_open(target, DISCONNECT_PATH),
                match target {
                    Target::Here => String::new(),
                    Target::Linked(_) => approve_inputs(target),
                },
                escape(&binding.id),
                match target {
                    Target::Here => "Disconnect",
                    Target::Linked(_) => "Disconnect, with your passkey",
                }
            ));
        }
    }
    body
}

/// The form a prepared link asks for (`mode`), filled with the link's values, for `target`; `None` when the link names no
/// mode. The values are shown as the link gave them and checked when the form is posted, as typed ones are.
fn prepared(ctx: &Ctx<'_>, target: &Target, query: &HashMap<String, String>) -> Option<String> {
    let mode = field(query, "mode").to_ascii_lowercase();
    if mode.is_empty() {
        return None;
    }
    let repository = field(query, "repository");
    let org = match field(query, "org") {
        "" => repository.split('/').next().unwrap_or("").trim(),
        org => org,
    };
    let labels = field(query, "labels")
        .split(';')
        .map(str::trim)
        .filter(|set| !set.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let to = match target {
        Target::Here => "this node".to_string(),
        Target::Linked(node) => format!("the linked node {}", escape(node)),
    };
    let app_form = form_open(target, APP_PATH);
    let inputs = match target {
        Target::Here => String::new(),
        Target::Linked(_) => approve_inputs(target),
    };
    let go = match target {
        Target::Here => "Continue to GitHub".to_string(),
        Target::Linked(node) => format!(
            "Approve for {} with your passkey, and continue to GitHub",
            escape(node)
        ),
    };
    let org_input = format!(
        "<label for=\"l-org\">Organisation</label><input id=\"l-org\" name=\"org\" required maxlength=\"39\" value=\"{}\">",
        escape(org)
    );
    let labels_area = format!(
        "<label for=\"l-labels\">Labels: one set a line</label><textarea id=\"l-labels\" name=\"labels\" rows=\"2\" required>{}</textarea>",
        escape(&labels)
    );
    let intro =
        "<p>This link filled in the form. Check the values, change any of them, and go on.</p>";
    let section = match (mode.as_str(), target) {
        ("app", _) => format!(
            "<section id=\"prepared\"><h2>Arca's App in {} for {to}</h2>{intro}{app_form}\
             {inputs}{org_input}{labels_area}<button type=\"submit\">{go}</button></form></section>",
            escape(org)
        ),
        ("repositories" | "repository", _) => format!(
            "<section id=\"prepared\"><h2>Workspace repositories of {} for {to}</h2>{intro}{}\
             {app_form}{inputs}<input type=\"hidden\" name=\"purpose\" value=\"repositories\">\
             {org_input}<button type=\"submit\">{go}</button></form></section>",
            escape(org),
            if repository.contains('/') {
                format!("<p>On GitHub, select {} among the repositories.</p>", escape(repository))
            } else {
                String::new()
            }
        ),
        ("push", Target::Here) => format!(
            "<section id=\"prepared\"><h2>Your service sends {}'s runners to {to}</h2>{intro}\
             <form method=\"post\" action=\"{PUSH_PATH}\">{org_input}{labels_area}\
             <button type=\"submit\">Make the address and token</button></form></section>",
            escape(org)
        ),
        ("oauth" | "device", Target::Here) if ctx.signer.oauth_client_id().is_some() => format!(
            "<section id=\"prepared\"><h2>Sign in with GitHub for {}'s runners on {to}</h2>{intro}\
             <form method=\"post\" action=\"{DEVICE_PATH}\">{org_input}\
             <label for=\"l-group\">Runner group (empty: the default group)</label><input id=\"l-group\" name=\"group\" maxlength=\"100\" value=\"{}\">\
             <label for=\"l-dlabels\">Labels, as your workflows' runs-on names them</label><input id=\"l-dlabels\" name=\"labels\" required value=\"{}\">\
             <label for=\"l-runners\">How many runners</label><input id=\"l-runners\" name=\"runners\" type=\"number\" min=\"1\" max=\"50\" value=\"{}\">\
             <button type=\"submit\">Get a code</button></form></section>",
            escape(org),
            escape(field(query, "group")),
            escape(&labels.replace('\n', ", ")),
            escape(match field(query, "runners") {
                "" => "2",
                runners => runners,
            }),
        ),
        ("oauth" | "device", Target::Here) => "<section id=\"prepared\"><h2>Sign in with GitHub</h2><p>This link asks \
             for the sign-in with a code, and this Arca has no GitHub OAuth App yet. Whoever looks after it sets one with \
             github_oauth_app_set.</p></section>"
            .to_string(),
        ("push" | "oauth" | "device", Target::Linked(_)) => format!(
            "<section id=\"prepared\"><h2>Not through the link</h2><p>A linked node is connected from this page with \
             Arca's App (mode=app) or the workspace repositories App (mode=repositories). Pushed configurations and \
             the sign-in with a code connect only the node whose page this is. Nothing was begun for {to}.</p></section>"
        ),
        (other, _) => format!(
            "<section id=\"prepared\"><h2>This link's mode is unknown</h2><p>It asks for \"{}\"; a prepared link's \
             mode is app, repositories, push or oauth. Nothing was begun.</p></section>",
            escape(other)
        ),
    };
    Some(section)
}

/// The words that open the page, and the line that names the node it connects.
fn page_opening(ctx: &Ctx<'_>, target: &Target) -> String {
    let mut body = String::from(
        "<h1>GitHub runners</h1><p>Connect a GitHub organisation, and its Actions jobs can run on this Arca. \
         Arca keeps the App's key, the sign-in grant and GitHub's tokens to itself. Each runner's own configuration, its \
         credential for one job, goes to the machine that runs it, and also to any assistant that holds an Arca \
         operator token and asks for one.</p>",
    );
    body.push_str(&match target {
        Target::Here => format!(
            "<p class=\"node\">You are connecting this node, {}.</p>",
            escape(ctx.here)
        ),
        Target::Linked(node) => format!(
            "<p class=\"node\">You are connecting the linked node {node}, from the page of {here}. Your passkey approves \
             each start and each disconnect, and {node} checks it itself. GitHub sends you back here; the App's key and \
             the connections are kept on {node}, not on {here}.</p>",
            node = escape(node),
            here = escape(ctx.here)
        ),
    });
    body.push_str(&passkey_ids(ctx));
    body
}

/// The signed-in page: the node it connects, the connections, an open flow when `flow` names one, a prepared link's form
/// when `mode` names one, and otherwise the ways to connect. A linked node is not asked anything here: its connections
/// are read when the owner asks for them.
pub async fn view(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, query) {
        Ok(target) => target,
        Err(refusal) => return *refusal,
    };
    let signer = ctx.signer;
    let mut body = page_opening(ctx, &target);
    let mut refresh = false;
    if target == Target::Here
        && let Some(flow) = query.get("flow").and_then(|id| signer.oauth_flow(id))
    {
        refresh = matches!(
            flow.state,
            FlowState::WaitingForPerson | FlowState::Registering
        );
        body.push_str("<section>");
        match flow.state {
            FlowState::WaitingForPerson => body.push_str(&format!(
                "<h2>Type this code on GitHub</h2><p class=\"code\">{}</p><p>Open <a href=\"{uri}\" target=\"_blank\" \
                 rel=\"noopener noreferrer\">{uri}</a>, type the code, and authorise. If your organisation restricts \
                 OAuth Apps, GitHub shows it with a Grant button on that screen: an owner of {} clicks Grant.</p>",
                escape(&flow.user_code),
                escape(&flow.account),
                uri = escape(&flow.verification_uri),
            )),
            FlowState::Registering => body.push_str("<h2>Registering your runners</h2>"),
            FlowState::Done => body.push_str("<h2 class=\"ok\">Done</h2>"),
            FlowState::Failed => body.push_str("<h2 class=\"bad\">Stopped</h2>"),
        }
        body.push_str("<ol class=\"steps\">");
        for step in &flow.steps {
            body.push_str(&format!(
                "<li class=\"{}\">{} <span class=\"mono\">{}</span></li>",
                if step.ok { "" } else { "bad" },
                escape(&step.text),
                escape(&step.at_text)
            ));
        }
        body.push_str("</ol></section>");
    }
    if let Some(id) = query.get("connected") {
        match &target {
            Target::Here => {
                if let Some(binding) = signer.bindings().iter().find(|binding| binding.id == *id) {
                    let words = if binding.repository_access {
                        "Its repository App supplies each workspace with access to its own selected repository."
                    } else {
                        "Runners register in its runner group with the labels you chose."
                    };
                    body.push_str(&format!(
                        "<section><h2 class=\"ok\">Connected</h2><p>{} is connected. {words}</p></section>",
                        escape(&binding.org_url(signer.endpoints()))
                    ));
                }
            }
            Target::Linked(node) => body.push_str(&format!(
                "<section><h2 class=\"ok\">Connected</h2><p>{} recorded the connection {}. Its runners register in its \
                 runner group with the labels you chose.</p></section>",
                escape(node),
                escape(id)
            )),
        }
    }

    let prepared = prepared(ctx, &target, query);
    if let Some(section) = &prepared {
        body.push_str(section);
    }

    body.push_str("<section><h2>Connections</h2>");
    match &target {
        Target::Here => {
            let bindings = signer.bindings();
            if bindings.is_empty() {
                body.push_str("<p>None yet.</p>");
            }
            body.push_str(&connection_rows(&bindings, &target));
        }
        Target::Linked(node) => body.push_str(&format!(
            "<p>{node} keeps its own connections.</p><form method=\"post\" action=\"{CONNECTIONS_PATH}\">{}\
             <button class=\"quiet\" type=\"submit\">Show the connections on {node}</button></form>",
            node_input(&target),
            node = escape(node)
        )),
    }
    body.push_str("</section>");

    if target == Target::Here {
        let flows = signer.oauth_flows();
        if !flows.is_empty() {
            body.push_str("<section><h2>Sign-ins of the last day</h2><ul>");
            for flow in flows.iter().take(10) {
                body.push_str(&format!(
                    "<li><a href=\"{PAGE_PATH}?flow={}\">{}</a>: {:?}</li>",
                    escape(&flow.id),
                    escape(&flow.account),
                    flow.state
                ));
            }
            body.push_str("</ul></section>");
        }
    }

    if prepared.is_some() {
        body.push_str(&format!(
            "<p><a href=\"{}\">Other ways to connect</a></p>",
            escape(&page_for(&target, ""))
        ));
        return html(&body, refresh);
    }

    let app_form = form_open(&target, APP_PATH);
    let inputs = match &target {
        Target::Here => String::new(),
        Target::Linked(_) => approve_inputs(&target),
    };
    let (repo_go, app_go) = match &target {
        Target::Here => (
            "Connect workspace repositories".to_string(),
            "Continue to GitHub".to_string(),
        ),
        Target::Linked(node) => (
            format!(
                "Approve for {} with your passkey, and continue to GitHub",
                escape(node)
            ),
            format!(
                "Approve for {} with your passkey, and continue to GitHub",
                escape(node)
            ),
        ),
    };
    body.push_str(&format!(
        "<section><h2>Workspace repositories</h2><p>Create a separate repository App for workspace Git access. It can fetch, push and open draft pull requests in the repositories you select. It does not change branch policies.</p>{app_form}{inputs}<input type=\"hidden\" name=\"purpose\" value=\"repositories\"><label for=\"repo-org\">Organisation</label><input id=\"repo-org\" name=\"org\" required maxlength=\"39\"><button type=\"submit\">{repo_go}</button></form></section>"
    ));
    if target != Target::Here {
        // Only the App's forms go through the link.
    } else if signer.oauth_client_id().is_some() {
        body.push_str(&format!(
            "<section><h2>Sign in with GitHub</h2><p>Arca shows a code; you type it on GitHub and allow Arca to \
             manage the organisation once (GitHub calls it admin:org). Arca uses that once to register the runners, \
             each with its own key, then asks GitHub to delete it, and shows you whether GitHub confirmed.</p>\
             <form method=\"post\" action=\"{DEVICE_PATH}\">\
             <label for=\"d-org\">Organisation</label><input id=\"d-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
             <label for=\"d-group\">Runner group (empty: the default group)</label><input id=\"d-group\" name=\"group\" maxlength=\"100\">\
             <label for=\"d-labels\">Labels, as your workflows' runs-on names them</label><input id=\"d-labels\" name=\"labels\" required placeholder=\"self-hosted, linux, x64\">\
             <label for=\"d-runners\">How many runners</label><input id=\"d-runners\" name=\"runners\" type=\"number\" min=\"1\" max=\"50\" value=\"2\">\
             <button type=\"submit\">Get a code</button></form></section>"
        ));
    } else {
        body.push_str(
            "<section><h2>Sign in with GitHub</h2><p>This Arca has no GitHub OAuth App yet. Whoever looks after it \
             sets one with github_oauth_app_set.</p></section>",
        );
    }
    let inputs = match &target {
        Target::Here => String::new(),
        Target::Linked(_) => {
            approve_inputs(&target).replace("class=\"note\"", "class=\"note\" id=\"a-note\"")
        }
    };
    body.push_str(&format!(
        "<section><h2>One click: Arca's App in your organisation</h2><p>GitHub opens with an App ready to create in \
         your organisation. It asks for one permission, Self-hosted runners: Read and write, and nothing else: no \
         code, no settings. GitHub cannot narrow that permission to one runner group; Arca holds itself to the group \
         it makes for the repositories you pick. You must be an owner of the organisation.</p>\
         {app_form}{inputs}\
         <label for=\"a-org\">Organisation</label><input id=\"a-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
         <label for=\"a-labels\">Labels: one set a line</label><textarea id=\"a-labels\" name=\"labels\" rows=\"2\" required placeholder=\"self-hosted, linux, x64\"></textarea>\
         <button type=\"submit\">{app_go}</button></form></section>"
    ));
    if target == Target::Here {
        body.push_str(&format!(
            "<section><h2>Your service sends the runners</h2><p>For an organisation that gives Arca no GitHub access at \
             all: your own service makes each runner's just-in-time configuration and sends it to Arca. Arca shows the \
             address and a token once.</p>\
             <form method=\"post\" action=\"{PUSH_PATH}\">\
             <label for=\"p-org\">Organisation</label><input id=\"p-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
             <label for=\"p-labels\">Labels: one set a line</label><textarea id=\"p-labels\" name=\"labels\" rows=\"2\" required placeholder=\"self-hosted, linux, x64\"></textarea>\
             <button type=\"submit\">Make the address and token</button></form></section>"
        ));
    } else {
        body.push_str(
            "<section><h2>Other ways</h2><p>Pushed configurations and the sign-in with a code connect only the node \
             whose page this is; a linked node is connected with one of the Apps above.</p></section>",
        );
    }
    html(&body, refresh)
}

/// `POST /approve/github/connections`: a linked node's connections, read now, each with its Disconnect, which the owner's
/// passkey approves.
pub async fn connections(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, form) {
        Ok(target) => target,
        Err(refusal) => return *refusal,
    };
    let Target::Linked(node) = &target else {
        return see_other(PAGE_PATH);
    };
    let mut body = page_opening(ctx, &target);
    body.push_str(&format!(
        "<section><h2>Connections on {}</h2>",
        escape(node)
    ));
    match ctx
        .links
        .link(node, json!({ "step": "bindings", "origin": ctx.origin }))
        .await
    {
        Ok(answer) => {
            let bindings = serde_json::from_value::<Vec<Binding>>(answer["bindings"].clone())
                .unwrap_or_default();
            if bindings.is_empty() {
                body.push_str("<p>None yet.</p>");
            }
            body.push_str(&connection_rows(&bindings, &target));
        }
        Err(error) => body.push_str(&format!(
            "<p class=\"bad\">{}</p>",
            escape(&relay_words(node, &error))
        )),
    }
    body.push_str(&format!(
        "</section><p><a href=\"{}\">Back</a></p>",
        escape(&page_for(&target, ""))
    ));
    html(&body, false)
}

/// `POST /approve/github/device`: starts a device flow and shows its code.
pub async fn device(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let runners = field(form, "runners").parse::<u32>().unwrap_or(0);
    let group = Some(field(form, "group").to_string()).filter(|group| !group.is_empty());
    let request = OauthRequest {
        account: field(form, "org").to_string(),
        group,
        labels: label_set(field(form, "labels")),
        runners,
        name_prefix: None,
        caller: ctx.caller.to_string(),
    };
    match ctx.signer.oauth_start(request).await {
        Ok(flow) => see_other(&format!("{PAGE_PATH}?flow={}", flow.id)),
        Err(StartError::Signer(error)) => {
            message_page(StatusCode::BAD_REQUEST, "Not started", &error.message())
        }
        Err(StartError::GitHub(why)) => message_page(
            StatusCode::BAD_GATEWAY,
            "GitHub did not start the sign-in",
            &why,
        ),
    }
}

/// The page that sends a manifest to GitHub: a form posted to `action_url` with the manifest, for `target`. Its CSP lets
/// one form go to GitHub's web host.
fn manifest_page(
    ctx: &Ctx<'_>,
    target: &Target,
    org: &str,
    repositories: bool,
    action_url: &str,
    manifest: &str,
) -> Response {
    let kept = match target {
        Target::Here => String::new(),
        Target::Linked(node) => format!(
            "<p>The App is for the linked node {node}: GitHub sends you back to this page, and {node} keeps its key.</p>",
            node = escape(node)
        ),
    };
    let body = if repositories {
        format!(
            "<h1>Create the repository App</h1>{kept}<p>GitHub will suggest a unique repository App name in {}. You may edit the name on GitHub. It asks for Contents and Pull requests: Read and write, and Metadata: Read. No webhook. Choose Only select repositories, select the workspace repositories and install. Existing branch policies stay in place.</p><form method=\"post\" action=\"{}\"><input type=\"hidden\" name=\"manifest\" value=\"{}\"><button type=\"submit\">Create the App on GitHub</button></form>",
            escape(org),
            escape(action_url),
            escape(manifest)
        )
    } else {
        format!(
            "<h1>Create the App on GitHub</h1>{kept}<section><p>GitHub will show an App named for Arca, owned by {org}, asking \
             for Self-hosted runners: Read and write and nothing else. Click Create there. GitHub then asks where to \
             install it: choose Only select repositories, pick the ones whose jobs may run here, and Install. You come \
             back here when it is done.</p>\
             <form method=\"post\" action=\"{action}\"><input type=\"hidden\" name=\"manifest\" value=\"{manifest}\">\
             <button type=\"submit\">Create the App on GitHub</button></form></section>",
            org = escape(org),
            action = escape(action_url),
            manifest = escape(manifest),
        )
    };
    let mut response = html(&body, false);
    let web = web_origin(&ctx.signer.endpoints().web);
    let csp =
        crate::canvas::csp().replace("form-action 'self'", &format!("form-action 'self' {web}"));
    if let Ok(value) = HeaderValue::from_str(&csp) {
        response.headers_mut().insert(
            header::HeaderName::from_static("content-security-policy"),
            value,
        );
    }
    response
}

/// `POST /approve/github/app`: begins the manifest flow on this node, or, with the owner's approval the passkey gave, on
/// the linked node the form names, and shows the page that sends the manifest to GitHub.
pub async fn app_start(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, form) {
        Ok(target) => target,
        Err(refusal) => return *refusal,
    };
    let sets = label_sets(field(form, "labels"));
    let repositories = field(form, "purpose") == "repositories";
    let org = field(form, "org");
    let node = match &target {
        Target::Here => {
            let started = if repositories {
                ctx.signer.repository_app_begin(org, ctx.origin, ctx.caller)
            } else {
                ctx.signer.app_begin(org, &sets, ctx.origin, ctx.caller)
            };
            return match started {
                Ok(start) => manifest_page(
                    ctx,
                    &target,
                    org,
                    repositories,
                    &start.action_url,
                    &start.manifest,
                ),
                Err(error) => {
                    message_page(StatusCode::BAD_REQUEST, "Not started", &error.message())
                }
            };
        }
        Target::Linked(node) => node,
    };
    let result = match approved(ctx, node, form).await {
        Ok(result) => result,
        Err(refusal) => return *refusal,
    };
    let text = |key: &str| result[key].as_str().unwrap_or("").to_string();
    let (action_url, manifest, state) = (text("actionUrl"), text("manifest"), text("state"));
    if manifest.is_empty() || state.is_empty() || !on_github(ctx, &action_url) {
        return message_page(
            StatusCode::BAD_GATEWAY,
            "Not started",
            &format!(
                "The linked node {node} did not answer with a manifest for GitHub. Nothing was made on GitHub."
            ),
        );
    }
    let account = match text("account") {
        account if account.is_empty() => org.to_string(),
        account => account,
    };
    let repositories = result["purpose"] == "repositories";
    ctx.links.remember(
        &state,
        LinkedFlow {
            node: node.clone(),
            plan: field(form, "plan").to_string(),
            account: account.clone(),
            repositories,
            app_name: text("appName"),
            created_at: ctx.signer.now(),
            app: None,
            binding: None,
        },
    );
    manifest_page(ctx, &target, &account, repositories, &action_url, &manifest)
}

/// The owner's approval of the card the form's button prepared, relayed to the linked node `node`, which checks the
/// passkey's assertion itself and runs the approved step: what the step answered. A form that comes with no approval (a
/// browser that ran no script) is told so; nothing was asked of the node.
async fn approved(
    ctx: &Ctx<'_>,
    node: &str,
    form: &HashMap<String, String>,
) -> Result<Value, Box<Response>> {
    let (plan, approval) = (field(form, "plan"), field(form, "approval"));
    if plan.is_empty() || approval.is_empty() {
        return Err(Box::new(message_page(
            StatusCode::BAD_REQUEST,
            "Approve it with your passkey",
            &format!(
                "{node} takes this step only with your passkey's approval, which the button asks for. This page needs \
                 its script for that: allow it, reload, and press the button again. Nothing was done."
            ),
        )));
    }
    let answer = ctx
        .links
        .call(
            node,
            "action_approve",
            json!({ "plan": plan, "approval": approval }),
            crate::linked::timeout_for("action_approve"),
        )
        .await;
    match answer {
        Ok(answer) => Ok(answer["result"].clone()),
        Err(error) => Err(Box::new(message_page(
            relay_status(&error),
            "Not done",
            &if not_reached(&error) {
                format!(
                    "{} It may not have heard the approval, or its answer did not come back: open {node}'s connections \
                     on this page to see what it holds.",
                    relay_words(node, &error)
                )
            } else {
                format!("{} Nothing was made on GitHub.", relay_words(node, &error))
            },
        ))),
    }
}

/// The scheme, host and port of an address.
fn web_origin(url: &str) -> String {
    let after = url.find("://").map_or(0, |at| at + 3);
    match url[after..].find('/') {
        Some(slash) => url[..after + slash].to_string(),
        None => url.to_string(),
    }
}

/// Where the organisation `account`'s Apps are listed on GitHub, to delete one.
fn apps_settings(ctx: &Ctx<'_>, account: &str) -> String {
    format!(
        "{}/organizations/{account}/settings/apps",
        ctx.signer.endpoints().web
    )
}

/// `GET /approve/github/created`: GitHub's return after Create. A flow this page began on a linked node goes to that node,
/// which takes the App and its key itself. When its answer does not come back, the page says the outcome is not known yet
/// and asks the node what it holds; it never says nothing is stored unless the node said so.
pub async fn created(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let (code, state) = (field(query, "code"), field(query, "state"));
    let Some(flow) = ctx.links.flow(state, ctx.signer.now()) else {
        return match ctx.signer.app_created(code, state).await {
            Ok(install) => see_other(&install),
            Err(why) => message_page(StatusCode::BAD_REQUEST, "The App was not taken", &why),
        };
    };
    let node = &flow.node;
    let mut args = flow.bound(ctx.origin, state);
    args["step"] = json!("created");
    args["code"] = json!(code);
    let answer = match ctx.links.link(node, args).await {
        Ok(answer) => answer,
        Err(error) if not_reached(&error) => {
            return created_unknown(ctx, &flow, state, &error).await;
        }
        Err(error) => {
            return message_page(
                relay_status(&error),
                "The App was not taken",
                &format!(
                    "{} If GitHub made the App {} in {} and you do not go on, delete it on GitHub at {}.",
                    relay_words(node, &error),
                    flow.app_name,
                    flow.account,
                    apps_settings(ctx, &flow.account)
                ),
            );
        }
    };
    took(ctx, &flow, state, &answer)
}

/// The linked node took the App (its answer, or its flow read later): the index learns it, and the browser goes to the
/// App's install screen, when that is on GitHub's own host.
fn took(ctx: &Ctx<'_>, flow: &LinkedFlow, state: &str, answer: &Value) -> Response {
    let node = &flow.node;
    let install = answer["installUrl"].as_str().unwrap_or("").to_string();
    let slug = answer.pointer("/app/slug").and_then(Value::as_str);
    let settings = answer.pointer("/app/settingsUrl").and_then(Value::as_str);
    if let (Some(slug), Some(settings)) = (slug, settings) {
        ctx.links.note_app(node, slug);
        let app = (slug.to_string(), settings.to_string());
        ctx.links.update(state, |flow| flow.app = Some(app));
    }
    if let Some(binding) = answer["binding"].as_str().filter(|id| !id.is_empty()) {
        let binding = binding.to_string();
        ctx.links.update(state, |flow| flow.binding = Some(binding));
    }
    if !on_github(ctx, &install) {
        return message_page(
            StatusCode::BAD_GATEWAY,
            "Taken, not installed yet",
            &format!(
                "The linked node {node} took the App, but answered no install screen on GitHub, so this page sends you \
                 nowhere. Install the App from the organisation's settings on GitHub: {}.",
                apps_settings(ctx, &flow.account)
            ),
        );
    }
    see_other(&install)
}

/// GitHub's return whose answer did not come back from `flow`'s node: the outcome is not known yet, so the page asks the
/// node what it holds of the flow and says that.
async fn created_unknown(
    ctx: &Ctx<'_>,
    flow: &LinkedFlow,
    state: &str,
    error: &RelayError,
) -> Response {
    let node = &flow.node;
    let mut args = flow.bound(ctx.origin, state);
    args["step"] = json!("flow");
    let read = ctx
        .links
        .call(
            node,
            LINK_VERB,
            args,
            crate::linked::timeout_for("plan_list"),
        )
        .await;
    let delete = format!(
        "If you do not go on, delete the App {} on GitHub at {}.",
        flow.app_name,
        apps_settings(ctx, &flow.account)
    );
    match read {
        Ok(held) if held["binding"].is_string() => took(ctx, flow, state, &held),
        Ok(held) if held["known"] == json!(true) => message_page(
            StatusCode::BAD_GATEWAY,
            "Not known yet",
            &format!(
                "{} {node} says it has not taken the App {} yet, and it may still be doing so. Reload this page in a \
                 minute: GitHub's code lives an hour. {delete}",
                relay_words(node, error),
                flow.app_name
            ),
        ),
        Ok(_) => message_page(
            StatusCode::BAD_GATEWAY,
            "The App was not taken",
            &format!(
                "{} {node} says it no longer knows this flow, so it holds nothing of it. GitHub made the App {} in {}: \
                 delete it on GitHub at {}, and start again from this page.",
                relay_words(node, error),
                flow.app_name,
                flow.account,
                apps_settings(ctx, &flow.account)
            ),
        ),
        Err(_) => message_page(
            StatusCode::BAD_GATEWAY,
            "Not known yet",
            &format!(
                "{} Whether {node} took the App {} is not known yet. Reload this page once {node} answers, within the \
                 hour, and the connection goes on. {delete}",
                relay_words(node, error),
                flow.app_name
            ),
        ),
    }
}

/// `GET /approve/github/installed`: GitHub's return after Install (or a change of repositories). A flow this page began on
/// a linked node goes to that node. A return with no live flow (repositories changed, or an install finished after the
/// hour) acts only when `signed_in` (the owner's session on this page), at most once a minute per installation, and goes
/// to the node the index names for its installation, or to this node, or to the linked node whose App it is.
pub async fn installed(
    ctx: &Ctx<'_>,
    query: &HashMap<String, String>,
    signed_in: bool,
) -> Response {
    let Ok(installation) = field(query, "installation_id").parse::<u64>() else {
        return message_page(
            StatusCode::BAD_REQUEST,
            "Not installed",
            "GitHub's link named no installation.",
        );
    };
    let state = Some(field(query, "state")).filter(|state| !state.is_empty());
    let Some((state, flow)) =
        state.and_then(|state| Some((state, ctx.links.flow(state, ctx.signer.now())?)))
    else {
        // No live flow of this page's: the return address is public, so it acts only for the owner, signed in on this
        // page, and reads one installation at most once a minute.
        if !local_flow(ctx, state) {
            if !signed_in {
                return sign_in_first();
            }
            let Some(_reading) = ctx.links.start_reading(installation) else {
                return message_page(
                    StatusCode::ACCEPTED,
                    "Being read now",
                    "GitHub's change to this installation is being read now. Reload this page in a moment.",
                );
            };
            if ctx.links.read_lately(installation, ctx.signer.now()) {
                return message_page(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Read a moment ago",
                    "GitHub's change to this installation was read less than a minute ago. Reload this page in a \
                     minute to read it again.",
                );
            }
            return installed_later(ctx, installation, state).await;
        }
        return installed_later(ctx, installation, state).await;
    };
    let node = &flow.node;
    let mut args = flow.bound(ctx.origin, state);
    args["step"] = json!("installed");
    args["installationId"] = json!(installation);
    let (app, undo) = match &flow.app {
        Some((slug, settings)) => (
            format!("the App {slug}"),
            format!("uninstall and delete the App on GitHub at {settings}"),
        ),
        None => (
            format!("the App {}", flow.app_name),
            format!(
                "uninstall and delete the App on GitHub at {}",
                apps_settings(ctx, &flow.account)
            ),
        ),
    };
    let answer = match ctx.links.link(node, args).await {
        Ok(answer) => answer["binding"].as_str().map(str::to_string).ok_or(None),
        // No answer, or the node no longer knows the flow (an answer that came late finished it): what the node holds
        // is read before the page says anything.
        Err(error) => connection_for(ctx, &flow, installation)
            .await
            .ok_or(Some(error)),
    };
    let error = match answer {
        Ok(id) => {
            ctx.links.forget(state);
            ctx.links.note_installation(installation, node);
            return see_other(&page_for(
                &Target::Linked(node.clone()),
                &format!("connected={id}"),
            ));
        }
        Err(Some(error)) => error,
        Err(None) => RelayError::Failed("no connection in the answer".to_string()),
    };
    match error {
        error if not_reached(&error) => message_page(
            StatusCode::BAD_GATEWAY,
            "Installed, not known to be recorded yet",
            &format!(
                "{} GitHub installed {app} in {}, and {node} keeps its key; whether {node} has recorded the \
                 installation is not known yet. Reload this page once {node} answers to finish. To undo it instead, \
                 {undo}.",
                relay_words(node, &error),
                flow.account,
            ),
        ),
        error => message_page(
            relay_status(&error),
            "Not connected",
            &format!(
                "{} GitHub installed {app} in {}. To undo it, {undo}.",
                relay_words(node, &error),
                flow.account,
            ),
        ),
    }
}

/// The connection `node` holds for GitHub's installation `installation`, once it is in use: read from the node itself.
/// Whether `state` names a live App flow this node began for itself: its own return, which its state binds.
fn local_flow(ctx: &Ctx<'_>, state: Option<&str>) -> bool {
    state.is_some_and(|state| ctx.signer.app_flow(state).is_some())
}

/// Whether GitHub's return `query` belongs to a live flow this page began, on this node or a linked one: the only
/// returns that act without the owner's signed-in session.
pub fn live_return(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> bool {
    let state = Some(field(query, "state")).filter(|state| !state.is_empty());
    local_flow(ctx, state)
        || state.is_some_and(|state| ctx.links.flow(state, ctx.signer.now()).is_some())
}

/// The answer to a return with no live flow and no signed-in session: the same for every installation, naming none.
fn sign_in_first() -> Response {
    message_page(
        StatusCode::UNAUTHORIZED,
        "Sign in to finish",
        "Sign in on Arca's GitHub page with your passkey, then open GitHub's link again.",
    )
}

/// The flow's own connection, the one its node made for the flow's App, when that connection is in use with GitHub's
/// installation `installation`: read from the node itself. Another connection with that installation is not this flow's.
async fn connection_for(ctx: &Ctx<'_>, flow: &LinkedFlow, installation: u64) -> Option<String> {
    let own = flow.binding.as_deref()?;
    let answer = ctx
        .links
        .link(
            &flow.node,
            json!({ "step": "bindings", "origin": ctx.origin }),
        )
        .await
        .ok()?;
    answer["bindings"]
        .as_array()?
        .iter()
        .find(|binding| {
            binding["id"] == own
                && binding["installation"]["id"] == json!(installation)
                && binding["status"] == "active"
                && flow
                    .app
                    .as_ref()
                    .is_none_or(|(slug, _)| binding["app"]["slug"] == slug.as_str())
        })
        .map(|_| own.to_string())
}

/// Who claims GitHub's installation on a return with no flow: this node, or a linked node.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Claim {
    Here,
    Linked(String),
}

/// What one linked node said when asked whether one of its Apps holds an installation.
enum Said {
    /// It does, with this App (its name on GitHub).
    Holds(Option<String>),
    /// GitHub told it the installation is not its App's.
    NotHeld,
    /// It did not answer in time, or GitHub could not say.
    Undecided,
}

fn said(answer: Result<Value, RelayError>) -> Said {
    match answer {
        Ok(answer) if answer["binding"].as_str().is_some_and(|id| !id.is_empty()) => Said::Holds(
            answer
                .pointer("/app/slug")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        Ok(_) => Said::NotHeld,
        Err(RelayError::Refused { code, .. }) if code == "GitHubUnreachable" => Said::Undecided,
        Err(RelayError::Refused { .. }) => Said::NotHeld,
        Err(_) => Said::Undecided,
    }
}

/// GitHub's return with no flow this page began (only for the owner's session, one read at a time and a minute apart).
///
/// An installation the index names was made for that node's App in a flow this page relayed, and GitHub never moves an
/// installation to another App: that node alone decides. Otherwise this node and every linked node that holds an App
/// from this page are asked at the same time, within one bound, whether one of their Apps holds it (`claim`, writing
/// nothing); a linked node's claim counts only when it names an App this page relayed for that node. Exactly one claim,
/// and that node brings the installation up to date (`refresh`); two or more, and nothing is done and the owner is told
/// which nodes claim it. A node that does not answer, or whose GitHub could not say, decides nothing, so nothing is
/// done until it does. The read counts against the minute only once the update answered, or every node did. The index
/// is not written here.
async fn installed_later(ctx: &Ctx<'_>, installation: u64, state: Option<&str>) -> Response {
    let index = ctx.links.index();
    let indexed = index
        .installations
        .get(&installation)
        .filter(|node| ctx.links.names().contains(node))
        .cloned();
    let stale = index.installations.contains_key(&installation);
    let bound = ctx.links.claim_bound;
    // One hard deadline for every node asked: a node that keeps sending a byte at a time is cut off at it, and what it
    // answers later is dropped.
    let deadline = tokio::time::Instant::now() + bound;
    let by_deadline = |call: tokio::task::JoinHandle<Result<Value, RelayError>>| async move {
        match tokio::time::timeout_at(deadline, call).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) | Err(_) => Err(RelayError::Unreachable),
        }
    };
    let claim = json!({ "step": "claim", "origin": ctx.origin, "installationId": installation });

    if let Some(node) = indexed {
        let answer = by_deadline(ctx.links.start_call(&node, claim, bound)).await;
        return match said(answer) {
            Said::Holds(_) => update(ctx, installation, state, Claim::Linked(node), false).await,
            Said::NotHeld => {
                ctx.links.note_read(installation, ctx.signer.now());
                ctx.links.forget_installation(installation);
                message_page(
                    StatusCode::BAD_REQUEST,
                    "Not connected",
                    &format!(
                        "{node}, whose App GitHub made this installation for, says GitHub no longer holds it: the App \
                         was uninstalled or deleted. Nothing was changed. Connect the organisation again from this page."
                    ),
                )
            }
            Said::Undecided => not_decided(&[node]),
        };
    }

    let mut nodes: Vec<String> = Vec::new();
    for (node, _) in &index.apps {
        if !nodes.contains(node) && ctx.links.names().contains(node) {
            nodes.push(node.clone());
        }
    }
    let calls: Vec<_> = nodes
        .iter()
        .map(|node| ctx.links.start_call(node, claim.clone(), bound))
        .collect();
    let (local, answers) = tokio::join!(
        tokio::time::timeout_at(deadline, ctx.signer.app_owning(installation)),
        async {
            let mut answers = Vec::new();
            for call in calls {
                answers.push(by_deadline(call).await);
            }
            answers
        }
    );
    let mut claims = Vec::new();
    let mut silent = Vec::new();
    match local {
        Ok(Ok(Some(_))) => claims.push(Claim::Here),
        Ok(Ok(None)) => {}
        Ok(Err(_)) | Err(_) => silent.push(format!("this node ({})", ctx.here)),
    }
    for (node, answer) in nodes.into_iter().zip(answers) {
        match said(answer) {
            Said::Holds(Some(slug)) if index.apps.contains(&(node.clone(), slug.clone())) => {
                claims.push(Claim::Linked(node));
            }
            Said::Holds(_) | Said::NotHeld => {}
            Said::Undecided => silent.push(node),
        }
    }
    if !silent.is_empty() {
        return not_decided(&silent);
    }
    match claims.as_slice() {
        [] => {
            ctx.links.note_read(installation, ctx.signer.now());
            if stale {
                ctx.links.forget_installation(installation);
            }
            message_page(
                StatusCode::BAD_REQUEST,
                "Not connected",
                "No App Arca made answers for that installation. Start again from Arca's GitHub page.",
            )
        }
        [one] => update(ctx, installation, state, one.clone(), stale).await,
        many => {
            ctx.links.note_read(installation, ctx.signer.now());
            let names: Vec<String> = many
                .iter()
                .map(|claim| match claim {
                    Claim::Here => format!("this node ({})", ctx.here),
                    Claim::Linked(node) => node.clone(),
                })
                .collect();
            message_page(
                StatusCode::CONFLICT,
                "Claimed by more than one node",
                &format!(
                    "GitHub's installation {installation} is claimed by {}. Arca changed nothing on any of them. \
                     Disconnect it on the node that should not hold it, then reload this page.",
                    names.join(" and ")
                ),
            )
        }
    }
}

/// Nothing is decided on a return while `nodes` have not said whether they hold its installation.
fn not_decided(nodes: &[String]) -> Response {
    message_page(
        StatusCode::BAD_GATEWAY,
        "Not brought up to date yet",
        &format!(
            "{} did not say whether GitHub's installation is theirs (no answer in time, or GitHub could not say), so \
             nothing was changed. Reload this page in a moment.",
            nodes.join(" and ")
        ),
    )
}

/// The one node that claims the installation brings it up to date. The read counts against the minute once that
/// answered.
async fn update(
    ctx: &Ctx<'_>,
    installation: u64,
    state: Option<&str>,
    claim: Claim,
    stale: bool,
) -> Response {
    match claim {
        Claim::Here => match ctx.signer.app_installed(installation, state).await {
            Ok(id) => {
                ctx.links.note_read(installation, ctx.signer.now());
                if stale {
                    ctx.links.forget_installation(installation);
                }
                see_other(&format!("{PAGE_PATH}?connected={id}"))
            }
            Err(why) => message_page(StatusCode::BAD_GATEWAY, "Not brought up to date yet", &why),
        },
        Claim::Linked(node) => {
            let args =
                json!({ "step": "refresh", "origin": ctx.origin, "installationId": installation });
            match ctx.links.link(&node, args).await {
                Ok(answer) => {
                    ctx.links.note_read(installation, ctx.signer.now());
                    if stale {
                        ctx.links.forget_installation(installation);
                    }
                    see_other(&page_for(
                        &Target::Linked(node),
                        &format!("connected={}", answer["binding"].as_str().unwrap_or("")),
                    ))
                }
                Err(error) => {
                    if !not_reached(&error) {
                        ctx.links.note_read(installation, ctx.signer.now());
                    }
                    message_page(
                        relay_status(&error),
                        "Not brought up to date yet",
                        &format!(
                            "{} GitHub changed the installation of {node}'s App; reload this page once {node} answers \
                             and it reads the change.",
                            relay_words(&node, &error)
                        ),
                    )
                }
            }
        }
    }
}

/// `POST /approve/github/push`: makes a push binding and shows its address and token once.
pub fn push(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let sets = label_sets(field(form, "labels"));
    match ctx
        .signer
        .push_create(field(form, "org"), &sets, ctx.caller)
    {
        Ok((binding, token)) => {
            let address = format!("{}/github/intake/{}", ctx.origin, binding.id);
            let body = format!(
                "<h1>Your service's address and token</h1><section><p>This token is shown this once. Keep it in your \
                 service's secrets. For each runner, your service calls GitHub's generate-jitconfig for {org} with \
                 its own credential, then sends the answer's encoded_jit_config here:</p>\
                 <p class=\"mono\">POST {address}<br>Authorization: Bearer {token}<br>Content-Type: application/json<br>\
                 {{\"encodedJitConfig\": \"...\", \"labels\": [\"{labels}\"]}}</p>\
                 <p>The labels must be one of the sets you chose. A configuration waits at most a day.</p>\
                 <p><a href=\"{PAGE_PATH}\">Back to GitHub runners</a></p></section>",
                org = escape(&binding.account),
                address = escape(&address),
                token = escape(token.expose()),
                labels = escape(&binding.label_sets[0].join("\", \"")),
            );
            html(&body, false)
        }
        Err(error) => message_page(StatusCode::BAD_REQUEST, "Not made", &error.message()),
    }
}

/// `POST /approve/github/disconnect`: on this node, or, with the owner's approval the passkey gave, on the linked node the
/// form names.
pub async fn disconnect(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, form) {
        Ok(target) => target,
        Err(refusal) => return *refusal,
    };
    let binding = field(form, "binding");
    match &target {
        Target::Here => match ctx.signer.revoke(binding, ctx.caller).await {
            Ok(_) => see_other(PAGE_PATH),
            Err(error) => message_page(
                StatusCode::BAD_REQUEST,
                "Not disconnected",
                &error.message(),
            ),
        },
        Target::Linked(node) => match approved(ctx, node, form).await {
            Ok(_) => see_other(&page_for(&target, "")),
            Err(refusal) => *refusal,
        },
    }
}

/// A JSON answer of the prepare step.
fn prepared_answer(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

fn prepare_refused(status: StatusCode, message: &str) -> Response {
    prepared_answer(status, json!({ "message": message }))
}

/// `POST /approve/github/prepare`: the card a linked node's form button approves. The form is checked as a typed one is;
/// the node is asked whether it takes GitHub connections from this page, then records a `github_link` proposal for
/// exactly what the form says (a begin, or a disconnect of one connection), and the page answers the proposal's id and
/// the statement the owner's passkey signs, which names the host the node's own statements carry and a fresh nonce.
/// Nothing is begun or disconnected here: the node does that once it has checked the passkey's assertion.
pub async fn prepare(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, form) {
        Ok(target) => target,
        Err(_) => {
            return prepare_refused(
                StatusCode::BAD_REQUEST,
                &not_linked_words(&ctx.links.names()),
            );
        }
    };
    let Target::Linked(node) = &target else {
        return prepare_refused(
            StatusCode::BAD_REQUEST,
            "This node's own forms are posted as they are: only a linked node's need its card.",
        );
    };
    let disconnect = !field(form, "binding").is_empty();
    let args = if disconnect {
        json!({ "step": "disconnect", "origin": ctx.origin, "binding": field(form, "binding") })
    } else {
        let org = field(form, "org");
        if let Err(error) = check_account(org) {
            return prepare_refused(StatusCode::BAD_REQUEST, &error.message());
        }
        if field(form, "purpose") == "repositories" {
            json!({ "step": "begin", "origin": ctx.origin, "org": org, "purpose": "repositories" })
        } else {
            let sets = label_sets(field(form, "labels"));
            if sets.is_empty() || sets.len() > 10 {
                return prepare_refused(StatusCode::BAD_REQUEST, "labels: one to ten label sets");
            }
            let sets = match sets
                .iter()
                .map(|set| check_labels("labels", set))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(sets) => sets,
                Err(error) => return prepare_refused(StatusCode::BAD_REQUEST, &error.message()),
            };
            json!({ "step": "begin", "origin": ctx.origin, "org": org, "purpose": "runners", "labelSets": sets })
        }
    };
    let nothing = if disconnect {
        "Nothing was disconnected."
    } else {
        "Nothing was made on GitHub."
    };
    let failed = |error: &RelayError| {
        prepare_refused(
            relay_status(error),
            &format!("{} {nothing}", relay_words(node, error)),
        )
    };
    // Whether the node takes GitHub connections from this page at all: an older Arca, a token that does not name the
    // verb, or a page it does not trust is said now, before any card is made.
    if let Err(error) = ctx
        .links
        .call(
            node,
            LINK_VERB,
            json!({ "step": "flow", "origin": ctx.origin }),
            crate::linked::timeout_for("plan_list"),
        )
        .await
    {
        return failed(&error);
    }
    let proposed = match ctx
        .links
        .call(
            node,
            "action_propose",
            json!({ "action": LINK_VERB, "args": args }),
            crate::linked::timeout_for("action_propose"),
        )
        .await
    {
        Ok(proposed) => proposed,
        Err(error) => return failed(&error),
    };
    let (Some(id), Some(digest)) = (proposed["id"].as_str(), proposed["argsDigest"].as_str())
    else {
        return prepare_refused(
            StatusCode::BAD_GATEWAY,
            &format!("The linked node {node} recorded no card. {nothing}"),
        );
    };
    // The host the node's own statements carry, as it lists its waiting cards.
    let host = match ctx
        .links
        .call(
            node,
            "plan_list",
            json!({}),
            crate::linked::timeout_for("plan_list"),
        )
        .await
    {
        Ok(listed) => listed["plans"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["id"] == json!(id))
            .and_then(|row| row["host"].as_str())
            .map(str::to_string),
        Err(error) => return failed(&error),
    };
    let Some(host) = host else {
        return prepare_refused(
            StatusCode::BAD_GATEWAY,
            &format!("The linked node {node} did not list the card it recorded. {nothing}"),
        );
    };
    let statement = crate::handlers::fresh_nonce().ok().and_then(|nonce| {
        crate::handlers::action_statement(&host, id, LINK_VERB, digest, ctx.signer.now(), &nonce)
            .ok()
    });
    match statement {
        Some(statement) => prepared_answer(
            StatusCode::OK,
            json!({ "plan": id, "statement": statement, "summary": proposed["summary"] }),
        ),
        None => prepare_refused(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("This page could not write the card's statement. {nothing}"),
        ),
    }
}

/// The push intake's router. No page rule: a bearer per binding, and no browser.
pub fn intake_router(signer: Signer) -> Router {
    Router::new()
        .route(INTAKE_PATH, post(intake))
        .with_state(signer)
}

async fn intake(
    State(signer): State<Signer>,
    Path(binding): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let answer = |status: StatusCode, body: Value| (status, axum::Json(body)).into_response();
    if headers.contains_key(header::ORIGIN) {
        return answer(
            StatusCode::FORBIDDEN,
            json!({ "error": "not from a browser" }),
        );
    }
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return answer(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "a bearer token is required" }),
        );
    };
    let body: Bytes = match to_bytes(request.into_body(), 128 * 1024).await {
        Ok(body) => body,
        Err(_) => {
            return answer(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "error": "at most 128 KiB" }),
            );
        }
    };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return answer(
            StatusCode::BAD_REQUEST,
            json!({ "error": "the body is JSON" }),
        );
    };
    match signer.intake(&binding, token.trim(), &body) {
        Ok(queued) => answer(StatusCode::ACCEPTED, json!({ "queued": queued })),
        Err(refused) => answer(
            StatusCode::from_u16(refused.status).unwrap_or(StatusCode::BAD_REQUEST),
            json!({ "error": refused.why }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_decoded_and_labels_are_split_into_sets() {
        let form = parse_form("org=acme&labels=self-hosted%2C+linux%0D%0Abig&runners=2&x=%zz");
        assert_eq!(form["org"], "acme");
        assert_eq!(form["runners"], "2");
        assert_eq!(form["x"], "%zz");
        assert_eq!(
            label_sets(&form["labels"]),
            vec![
                vec!["self-hosted".to_string(), "linux".to_string()],
                vec!["big".to_string()]
            ]
        );
        assert_eq!(
            escape("<a href=\"x\">'"),
            "&lt;a href=&quot;x&quot;&gt;&#39;"
        );
        assert_eq!(web_origin("https://github.com"), "https://github.com");
        assert_eq!(web_origin("http://127.0.0.1:9/web"), "http://127.0.0.1:9");
    }

    /// Issue 328: a row says what the runner group is open to, and a connection whose own group is open to no
    /// repository is not shown as simply connected.
    #[test]
    fn a_row_says_what_the_runner_group_is_open_to() {
        use crate::github_signer::{GroupAccess, InstallationRecord, RunnerGroup};
        let binding = |access: Option<GroupAccess>, repositories: &[&str]| Binding {
            id: "gh-0123456789abcdef".to_string(),
            mode: Mode::App,
            repository_access: false,
            account: "acme".to_string(),
            status: Status::Active,
            why: None,
            label_sets: vec![vec!["linux".to_string()]],
            runner_group: Some(RunnerGroup {
                id: 20,
                name: "Arca runners".to_string(),
                created_by_arca: true,
                warning: None,
                repositories: None,
                access,
            }),
            runners: Vec::new(),
            app: None,
            installation: Some(InstallationRecord {
                id: 5,
                repository_selection: "selected".to_string(),
                repositories: repositories.iter().map(|name| name.to_string()).collect(),
            }),
            sync_error: None,
            sync_retry_in_ms: None,
            created_by: "page".to_string(),
            created_at: 0,
            updated_at: 0,
        };

        let row = connection_rows(
            &[binding(Some(GroupAccess::AllPrivate), &[])],
            &Target::Here,
        );
        assert!(row.contains("<span class=\"ok\">connected</span>"), "{row}");
        assert!(
            row.contains(
                "Every private and internal repository of acme may send jobs here. Narrow it in the runner group \
                 on GitHub."
            ),
            "{row}"
        );

        let row = connection_rows(
            &[binding(Some(GroupAccess::Selected), &["acme/web"])],
            &Target::Here,
        );
        assert!(row.contains("<span class=\"ok\">connected</span>"), "{row}");
        assert!(row.contains("acme/web"), "{row}");

        // A record from before issue 328: Arca's group, an installation listing nothing, no access recorded.
        let row = connection_rows(&[binding(None, &[])], &Target::Here);
        assert!(!row.contains("<span class=\"ok\">"), "{row}");
        assert!(row.contains("open to no repository"), "{row}");

        // A group whose own warning says what it is open to (a signed-in connection's) says it once.
        let mut warned = binding(Some(GroupAccess::AllPrivate), &[]);
        warned.runner_group.as_mut().unwrap().warning =
            Some("Arca made this group open.".to_string());
        let row = connection_rows(&[warned], &Target::Here);
        assert!(row.contains("Arca made this group open."), "{row}");
        assert!(!row.contains("Every private and internal"), "{row}");

        // A failed check: not checked yet, with the reason.
        let mut failed = binding(Some(GroupAccess::Selected), &["acme/web"]);
        failed.sync_error = Some("GitHub answered 502".to_string());
        let row = connection_rows(&[failed], &Target::Here);
        assert!(
            row.contains("<span class=\"bad\">connected, runner group not checked yet</span>"),
            "{row}"
        );
        assert!(row.contains("GitHub answered 502"), "{row}");
    }
}
