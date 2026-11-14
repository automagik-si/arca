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

use std::collections::HashMap;

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::github_oauth::{FlowState, OauthRequest, StartError};
use crate::github_signer::{Binding, Mode, Signer, Status};

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

/// The signed-in page: the connections, an open flow when `flow` names one, and the three ways to connect.
pub fn view(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let signer = ctx.signer;
    let mut body = String::from(
        "<h1>GitHub runners</h1><p>Connect a GitHub organisation, and its Actions jobs can run on this Arca. \
         Arca keeps the App's key, the sign-in grant and GitHub's tokens to itself. Each runner's own configuration, its \
         credential for one job, goes to the machine that runs it, and also to any assistant that holds an Arca \
         operator token and asks for one.</p>",
    );
    let mut refresh = false;
    if let Some(flow) = query.get("flow").and_then(|id| signer.oauth_flow(id)) {
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
        && let Ok(info) = signer.binding(id)
    {
        body.push_str(&format!(
            "<section><h2 class=\"ok\">Connected</h2><p>{} is connected. Runners register in its runner group with \
             the labels you chose.</p></section>",
            escape(&info.org_url)
        ));
    }

    let bindings = signer.bindings();
    body.push_str("<section><h2>Connections</h2>");
    if bindings.is_empty() {
        body.push_str("<p>None yet.</p>");
    }
    for binding in &bindings {
        let (tone, words) = status_words(binding);
        body.push_str(&format!(
            "<div class=\"row\"><strong>{}</strong><span class=\"{tone}\">{words}</span></div><p>{}",
            escape(&binding.account),
            mode_words(binding.mode)
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
                "<form method=\"post\" action=\"{DISCONNECT_PATH}\"><input type=\"hidden\" name=\"binding\" \
                 value=\"{}\"><button class=\"quiet\" type=\"submit\">Disconnect</button></form>",
                escape(&binding.id)
            ));
        }
    }
    body.push_str("</section>");

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

    if signer.oauth_client_id().is_some() {
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
         <form method=\"post\" action=\"{APP_PATH}\">\
         <label for=\"a-org\">Organisation</label><input id=\"a-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
         <label for=\"a-labels\">Labels: one set a line</label><textarea id=\"a-labels\" name=\"labels\" rows=\"2\" required placeholder=\"self-hosted, linux, x64\"></textarea>\
         <button type=\"submit\">Continue to GitHub</button></form></section>\
         <section><h2>Your service sends the runners</h2><p>For an organisation that gives Arca no GitHub access at \
         all: your own service makes each runner's just-in-time configuration and sends it to Arca. Arca shows the \
         address and a token once.</p>\
         <form method=\"post\" action=\"{PUSH_PATH}\">\
         <label for=\"p-org\">Organisation</label><input id=\"p-org\" name=\"org\" required maxlength=\"39\" placeholder=\"acme\">\
         <label for=\"p-labels\">Labels: one set a line</label><textarea id=\"p-labels\" name=\"labels\" rows=\"2\" required placeholder=\"self-hosted, linux, x64\"></textarea>\
         <button type=\"submit\">Make the address and token</button></form></section>"
    ));
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

/// `POST /approve/github/app`: the page that sends the manifest to GitHub. Its CSP lets one form go to GitHub's web
/// host.
pub fn app_start(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    let sets = label_sets(field(form, "labels"));
    let start = match ctx
        .signer
        .app_begin(field(form, "org"), &sets, ctx.origin, ctx.caller)
    {
        Ok(start) => start,
        Err(error) => {
            return message_page(StatusCode::BAD_REQUEST, "Not started", &error.message());
        }
    };
    let body = format!(
        "<h1>Create the App on GitHub</h1><section><p>GitHub will show an App named for Arca, owned by {org}, asking \
         for Self-hosted runners: Read and write and nothing else. Click Create there. GitHub then asks where to \
         install it: choose Only select repositories, pick the ones whose jobs may run here, and Install. You come \
         back here when it is done.</p>\
         <form method=\"post\" action=\"{action}\"><input type=\"hidden\" name=\"manifest\" value=\"{manifest}\">\
         <button type=\"submit\">Create the App on GitHub</button></form></section>",
        org = escape(field(form, "org")),
        action = escape(&start.action_url),
        manifest = escape(&start.manifest),
    );
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

/// The scheme, host and port of an address.
fn web_origin(url: &str) -> String {
    let after = url.find("://").map_or(0, |at| at + 3);
    match url[after..].find('/') {
        Some(slash) => url[..after + slash].to_string(),
        None => url.to_string(),
    }
}

/// `GET /approve/github/created`: GitHub's return after Create.
pub async fn created(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let (code, state) = (field(query, "code"), field(query, "state"));
    match ctx.signer.app_created(code, state).await {
        Ok(install) => see_other(&install),
        Err(why) => message_page(StatusCode::BAD_REQUEST, "The App was not taken", &why),
    }
}

/// `GET /approve/github/installed`: GitHub's return after Install (or a change of repositories).
pub async fn installed(ctx: &Ctx<'_>, query: &HashMap<String, String>) -> Response {
    let Ok(installation) = field(query, "installation_id").parse::<u64>() else {
        return message_page(
            StatusCode::BAD_REQUEST,
            "Not installed",
            "GitHub's link named no installation.",
        );
    };
    let state = Some(field(query, "state")).filter(|state| !state.is_empty());
    match ctx.signer.app_installed(installation, state).await {
        Ok(id) => see_other(&format!("{PAGE_PATH}?connected={id}")),
        Err(why) => message_page(StatusCode::BAD_REQUEST, "Not connected", &why),
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

/// `POST /approve/github/disconnect`.
pub async fn disconnect(ctx: &Ctx<'_>, form: &HashMap<String, String>) -> Response {
    match ctx.signer.revoke(field(form, "binding"), ctx.caller).await {
        Ok(_) => see_other(PAGE_PATH),
        Err(error) => message_page(
            StatusCode::BAD_REQUEST,
            "Not disconnected",
            &error.message(),
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
}
