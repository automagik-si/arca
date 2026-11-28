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
//! this page: the owner signs in here once, GitHub sends the browser back to this page's origin, and each step goes to the
//! node's `github_link` over the link (`crate::linked`). The linked node keeps the manifest flow and converts GitHub's
//! code into the App and its key itself, so the key never reaches this node: what passes through here is GitHub's
//! one-hour code, in one relayed call's body, never logged, journaled or shown. This node remembers, for an hour and in
//! memory only, which node each flow's `state` belongs to and the App's name, so a node that stops answering can be named
//! with what GitHub holds and where to delete it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::config::LinkedHost;
use crate::github_handlers::LINK_VERB;
use crate::github_oauth::{FlowState, OauthRequest, StartError};
use crate::github_signer::{Binding, Mode, Signer, Status};
use crate::linked::RelayError;

/// The page's own path, and its forms and returns.
pub const PAGE_PATH: &str = "/approve/github";
pub const DEVICE_PATH: &str = "/approve/github/device";
pub const APP_PATH: &str = "/approve/github/app";
pub const PUSH_PATH: &str = "/approve/github/push";
pub const DISCONNECT_PATH: &str = "/approve/github/disconnect";
pub const CREATED_PATH: &str = "/approve/github/created";
pub const INSTALLED_PATH: &str = "/approve/github/installed";
/// The push intake: `POST /github/intake/{binding}`.
pub const INTAKE_PATH: &str = "/github/intake/{binding}";

/// The page template. Its one `<style>` is hashed into the page origin's CSP.
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
}

/// How long the page remembers which linked node a manifest flow belongs to: GitHub's code lives as long.
const LINKED_FLOW_TTL_MS: u64 = 60 * 60 * 1000;

/// The relay to the linked nodes' `github_link`.
pub trait Relay: Send + Sync {
    /// The linked nodes' names.
    fn names(&self) -> Vec<String>;
    /// Calls `github_link` with `args` on the linked node `node`. Blocking.
    fn call(&self, node: &str, args: Value) -> Result<Value, RelayError>;
}

/// The relay over each linked host's own socket and token, as `crate::linked` makes every call.
struct HostsRelay(Vec<LinkedHost>);

impl Relay for HostsRelay {
    fn names(&self) -> Vec<String> {
        self.0.iter().map(|host| host.name.clone()).collect()
    }

    fn call(&self, node: &str, args: Value) -> Result<Value, RelayError> {
        let host = self
            .0
            .iter()
            .find(|host| host.name == node)
            .ok_or(RelayError::Unreachable)?;
        crate::linked::call(host, LINK_VERB, args)
    }
}

/// A manifest flow this page began on a linked node: what the owner is told if the node stops answering.
#[derive(Clone, Debug)]
struct LinkedFlow {
    node: String,
    account: String,
    app_name: String,
    created_at: u64,
    /// The App's name on GitHub and its settings page, once the node took it.
    app: Option<(String, String)>,
}

/// The linked nodes this page connects, and the manifest flows it began on them, by `state`. In memory only.
pub struct Links {
    relay: Arc<dyn Relay>,
    flows: Mutex<HashMap<String, LinkedFlow>>,
}

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
        }
    }

    fn names(&self) -> Vec<String> {
        self.relay.names()
    }

    async fn call(&self, node: &str, args: Value) -> Result<Value, RelayError> {
        let relay = Arc::clone(&self.relay);
        let node = node.to_string();
        tokio::task::spawn_blocking(move || relay.call(&node, args))
            .await
            .unwrap_or(Err(RelayError::Unreachable))
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
        &if names.is_empty() {
            "That is not a node linked to this one: no node is linked to it.".to_string()
        } else {
            format!(
                "That is not a node linked to this one. The linked nodes are: {}.",
                names.join(", ")
            )
        },
    )))
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

/// The page's own address for `target`, with `more` query pairs.
fn page_for(target: &Target, more: &str) -> String {
    match (target, more.is_empty()) {
        (Target::Here, true) => PAGE_PATH.to_string(),
        (Target::Here, false) => format!("{PAGE_PATH}?{more}"),
        (Target::Linked(node), true) => format!("{PAGE_PATH}?node={node}"),
        (Target::Linked(node), false) => format!("{PAGE_PATH}?node={node}&{more}"),
    }
}

/// Why a relayed step to `node` did not answer, in words.
fn relay_words(node: &str, error: &RelayError) -> String {
    match error {
        RelayError::Unreachable => format!("The linked node {node} could not be reached just now."),
        RelayError::Token => format!(
            "The linked node {node} does not take this node's token for GitHub connections: the relay token's tools \
             must name {LINK_VERB}."
        ),
        RelayError::Refused { message, .. } => {
            format!("The linked node {node} refused: {message}.")
        }
        RelayError::Failed(why) => why.clone(),
    }
}

/// The status of a relayed step that did not answer: arguments the node found invalid are the form's, the rest the
/// node's.
fn relay_status(error: &RelayError) -> StatusCode {
    match error {
        RelayError::Failed(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Whether the node may have run the step (it answered with a refusal), as against not reached at all.
fn not_reached(error: &RelayError) -> bool {
    matches!(error, RelayError::Unreachable | RelayError::Token)
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

/// The connection rows of `bindings`, each with its Disconnect form for `target`.
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
        if let Some(why) = &binding.why {
            body.push_str(&format!("<p>{}</p>", escape(why)));
        }
        if binding.status != Status::Revoked {
            body.push_str(&format!(
                "<form method=\"post\" action=\"{DISCONNECT_PATH}\">{}<input type=\"hidden\" name=\"binding\" \
                 value=\"{}\"><button class=\"quiet\" type=\"submit\">Disconnect</button></form>",
                node_input(target),
                escape(&binding.id)
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
    let node = node_input(target);
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
            "<section id=\"prepared\"><h2>Arca's App in {} for {to}</h2>{intro}<form method=\"post\" action=\"{APP_PATH}\">\
             {node}{org_input}{labels_area}<button type=\"submit\">Continue to GitHub</button></form></section>",
            escape(org)
        ),
        ("repositories" | "repository", _) => format!(
            "<section id=\"prepared\"><h2>Workspace repositories of {} for {to}</h2>{intro}{}\
             <form method=\"post\" action=\"{APP_PATH}\">{node}<input type=\"hidden\" name=\"purpose\" value=\"repositories\">\
             {org_input}<button type=\"submit\">Continue to GitHub</button></form></section>",
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

/// The signed-in page: the node it connects, the connections, an open flow when `flow` names one, a prepared link's form
/// when `mode` names one, and otherwise the ways to connect.
pub async fn view(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let target = match target_of(ctx, query) {
        Ok(target) => target,
        Err(refusal) => return *refusal,
    };
    let signer = ctx.signer;
    let (bindings, unreachable) = match &target {
        Target::Here => (signer.bindings(), None),
        Target::Linked(node) => {
            match ctx
                .links
                .call(node, json!({ "step": "bindings", "origin": ctx.origin }))
                .await
            {
                Ok(answer) => (
                    serde_json::from_value::<Vec<Binding>>(answer["bindings"].clone())
                        .unwrap_or_default(),
                    None,
                ),
                Err(error) => (Vec::new(), Some(relay_words(node, &error))),
            }
        }
    };
    let mut body = String::from(
        "<h1>GitHub runners</h1><p>Connect a GitHub organisation, and its Actions jobs can run on this Arca. \
         Arca keeps the App's key, the sign-in grant and GitHub's tokens to itself. Each runner's own configuration, its \
         credential for one job, goes to the machine that runs it, and also to any assistant that holds an Arca \
         operator token and asks for one.</p>",
    );
    body.push_str(&match &target {
        Target::Here => format!(
            "<p class=\"node\">You are connecting this node, {}.</p>",
            escape(ctx.here)
        ),
        Target::Linked(node) => format!(
            "<p class=\"node\">You are connecting the linked node {node}, from the page of {here}. GitHub sends you back \
             here; the App's key and the connections are kept on {node}, not on {here}.</p>",
            node = escape(node),
            here = escape(ctx.here)
        ),
    });
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
    if let Some(id) = query.get("connected")
        && let Some(binding) = bindings.iter().find(|binding| binding.id == *id)
    {
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

    let prepared = prepared(ctx, &target, query);
    if let Some(section) = &prepared {
        body.push_str(section);
    }

    body.push_str("<section><h2>Connections</h2>");
    match &unreachable {
        Some(words) => body.push_str(&format!("<p class=\"bad\">{}</p>", escape(words))),
        None if bindings.is_empty() => body.push_str("<p>None yet.</p>"),
        None => {}
    }
    body.push_str(&connection_rows(&bindings, &target));
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

    let node = node_input(&target);
    body.push_str(&format!(
        "<section><h2>Workspace repositories</h2><p>Create a separate repository App for workspace Git access. It can fetch, push and open draft pull requests in the repositories you select. It does not change branch policies.</p><form method=\"post\" action=\"{APP_PATH}\">{node}<input type=\"hidden\" name=\"purpose\" value=\"repositories\"><label for=\"repo-org\">Organisation</label><input id=\"repo-org\" name=\"org\" required maxlength=\"39\"><button type=\"submit\">Connect workspace repositories</button></form></section>"
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
    body.push_str(&format!(
        "<section><h2>One click: Arca's App in your organisation</h2><p>GitHub opens with an App ready to create in \
         your organisation. It asks for one permission, Self-hosted runners: Read and write, and nothing else: no \
         code, no settings. GitHub cannot narrow that permission to one runner group; Arca holds itself to the group \
         it makes for the repositories you pick. You must be an owner of the organisation.</p>\
         <form method=\"post\" action=\"{APP_PATH}\">{node}\
         <label for=\"a-org\">Organisation</label><input id=\"a-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
         <label for=\"a-labels\">Labels: one set a line</label><textarea id=\"a-labels\" name=\"labels\" rows=\"2\" required placeholder=\"self-hosted, linux, x64\"></textarea>\
         <button type=\"submit\">Continue to GitHub</button></form></section>"
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

/// `POST /approve/github/app`: begins the manifest flow on this node, or on the linked node the form names, and shows
/// the page that sends the manifest to GitHub.
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
    let args = json!({
        "step": "begin",
        "origin": ctx.origin,
        "org": org,
        "labelSets": sets,
        "purpose": if repositories { "repositories" } else { "runners" },
        "by": ctx.caller,
    });
    let answer = match ctx.links.call(node, args).await {
        Ok(answer) => answer,
        Err(error) => {
            let words = relay_words(node, &error);
            return message_page(
                relay_status(&error),
                "Not started",
                &if matches!(error, RelayError::Failed(_)) {
                    words
                } else {
                    format!("{words} Nothing was made on GitHub.")
                },
            );
        }
    };
    let text = |key: &str| answer[key].as_str().unwrap_or("").to_string();
    let (action_url, manifest, state) = (text("actionUrl"), text("manifest"), text("state"));
    if action_url.is_empty() || manifest.is_empty() || state.is_empty() {
        return message_page(
            StatusCode::BAD_GATEWAY,
            "Not started",
            &format!(
                "The linked node {node} answered without a manifest. Nothing was made on GitHub."
            ),
        );
    }
    ctx.links.remember(
        &state,
        LinkedFlow {
            node: node.clone(),
            account: org.to_string(),
            app_name: text("appName"),
            created_at: ctx.signer.now(),
            app: None,
        },
    );
    manifest_page(ctx, &target, org, repositories, &action_url, &manifest)
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
/// which takes the App and its key itself.
pub async fn created(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let (code, state) = (field(query, "code"), field(query, "state"));
    let Some(flow) = ctx.links.flow(state, ctx.signer.now()) else {
        return match ctx.signer.app_created(code, state).await {
            Ok(install) => see_other(&install),
            Err(why) => message_page(StatusCode::BAD_REQUEST, "The App was not taken", &why),
        };
    };
    let node = &flow.node;
    let args = json!({ "step": "created", "origin": ctx.origin, "state": state, "code": code });
    match ctx.links.call(node, args).await {
        Ok(answer) => {
            let install = answer["installUrl"].as_str().unwrap_or("").to_string();
            let slug = answer.pointer("/app/slug").and_then(Value::as_str);
            let settings = answer.pointer("/app/settingsUrl").and_then(Value::as_str);
            if let (Some(slug), Some(settings)) = (slug, settings) {
                let app = (slug.to_string(), settings.to_string());
                ctx.links.update(state, |flow| flow.app = Some(app));
            }
            if install.is_empty() {
                return message_page(
                    StatusCode::BAD_GATEWAY,
                    "The App was not taken",
                    &format!("The linked node {node} answered without the App's install screen."),
                );
            }
            see_other(&install)
        }
        Err(error) if not_reached(&error) => message_page(
            StatusCode::BAD_GATEWAY,
            "The App was made and not taken yet",
            &format!(
                "{} Nothing took the new App, so nothing of it is stored on {node} or on this node. GitHub has made \
                 the App {} in {}. Reload this page within the hour, once {node} answers, and the connection goes on; \
                 or delete the App on GitHub at {}.",
                relay_words(node, &error),
                flow.app_name,
                flow.account,
                apps_settings(ctx, &flow.account)
            ),
        ),
        Err(error) => message_page(
            relay_status(&error),
            "The App was not taken",
            &format!(
                "{} If GitHub made the App {} in {}, nothing of it is stored on {node} or on this node: delete it on \
                 GitHub at {}.",
                relay_words(node, &error),
                flow.app_name,
                flow.account,
                apps_settings(ctx, &flow.account)
            ),
        ),
    }
}

/// `GET /approve/github/installed`: GitHub's return after Install (or a change of repositories). A flow this page began on
/// a linked node goes to that node.
pub async fn installed(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
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
        return match ctx.signer.app_installed(installation, state).await {
            Ok(id) => see_other(&format!("{PAGE_PATH}?connected={id}")),
            Err(why) => message_page(StatusCode::BAD_REQUEST, "Not connected", &why),
        };
    };
    let node = &flow.node;
    let args = json!({
        "step": "installed",
        "origin": ctx.origin,
        "state": state,
        "installationId": installation,
    });
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
    match ctx.links.call(node, args).await {
        Ok(answer) => {
            ctx.links.forget(state);
            let id = answer["binding"].as_str().unwrap_or("");
            see_other(&page_for(
                &Target::Linked(node.clone()),
                &format!("connected={id}"),
            ))
        }
        Err(error) if not_reached(&error) => message_page(
            StatusCode::BAD_GATEWAY,
            "Installed, not recorded yet",
            &format!(
                "{} GitHub installed {app} in {}, and {node} keeps its key, but {node} has not recorded the \
                 installation. Reload this page once {node} answers to finish. To undo it instead, {undo}.",
                relay_words(node, &error),
                flow.account,
            ),
        ),
        Err(error) => message_page(
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

/// `POST /approve/github/disconnect`: on this node, or on the linked node the form names.
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
        Target::Linked(node) => {
            let args = json!({
                "step": "disconnect",
                "origin": ctx.origin,
                "binding": binding,
                "by": ctx.caller,
            });
            match ctx.links.call(node, args).await {
                Ok(_) => see_other(&page_for(&target, "")),
                Err(error) => message_page(
                    relay_status(&error),
                    "Not disconnected",
                    &relay_words(node, &error),
                ),
            }
        }
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
}
