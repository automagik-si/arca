//! The HTTP surface: the `POST /mcp` route, the read-only GET routes, the unix socket listener, the tailnet listener with
//! its peer filter, the proxy listener with its exact-peer filter, and the services socket.
//!
//! `POST /mcp` answers `200` with a JSON-RPC result or error, `202` to a notification, and to a request that declares
//! protocol version `2026-07-28`: `400` when its headers and its body disagree or lack what that version requires, and
//! `404` for a method the server does not have. A `GET` on `/mcp` is `405`, and no session id is ever issued or read.
//!
//! A request carrying any `Origin` header is refused `403` before its body is read, whatever its method or path: a browser
//! page is never a client. The unix socket is bound inside a fresh private directory, chmodded 0600 and renamed into place,
//! so no other user ever reaches it at a wider mode, and it serves only peers of this process's own uid. The tailnet
//! listener binds one address that lies in the tailnet ranges, and every TCP peer is checked against the same ranges; a
//! peer outside them is dropped before any byte is read or written. The proxy listener binds one fixed address outside the
//! tailnet ranges for a reverse proxy on the same host, and serves exactly one peer address; any other peer is dropped the
//! same way. The proxy address may not be assigned yet when the server starts, so its bind is tried again for a while. The
//! loopback listener binds one loopback address for clients on the same machine, serves loopback peers only, and answers
//! only a request whose `Host` is the listener's own address or `localhost` with its port: a name that merely resolves to
//! loopback (DNS rebinding) is refused `403` on every route. Its address is a port any program of the machine can take
//! first, so a bind that fails never stops the other listeners: it is reported and tried again for as long as the server
//! runs. A peer there has a bounded time to send each request's head. The
//! services socket is a second unix socket, bound the same way as the first, that also serves root: Incus connects to it
//! for a service with kernel access. Each listener serves at most [`MAX_CONNECTIONS`] connections at once, counted for
//! that listener alone.

use std::collections::HashMap;
use std::fs;
use std::io::{self, IoSlice};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use arca_api::{Scope, Verb, may_call};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::StreamExt as _;

use crate::auth::{Caller, IssuedTokens, TokenStore};
use crate::elicit::TerminalApprover;
use crate::fresh::{fresh_dir, process_uid};
use crate::jsonrpc;
use crate::tools::{CallError, ToolHandler};
use crate::version::{Mirrored, Mirrors};

/// The most body bytes one request may carry.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// The most connections one listener serves at once. A connection past the cap waits in the kernel's backlog.
const MAX_CONNECTIONS: usize = 64;
/// The ports a tailnet or proxy listener may never take: the ones other services of the node use.
const RESERVED_PORTS: [u16; 4] = [80, 443, 7443, 9443];
/// How long a listener waits after a failed accept before it tries again.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
/// How the loopback listener tries again to bind an address it could not take, such as a port another program holds:
/// for as long as the server runs, pausing 1 s, then twice as long each time, up to 30 s.
const LOOPBACK_BIND_FIRST: Duration = Duration::from_secs(1);
const LOOPBACK_BIND_MAX: Duration = Duration::from_secs(30);
/// How long a peer of the loopback listener may take to send a request's head: the first of a connection, and each
/// next one on a connection that is kept open. A peer that sends nothing in that time is closed, so idle connections
/// cannot hold the listener's [`MAX_CONNECTIONS`] slots.
const LOOPBACK_HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How the proxy listener tries again to bind an address that is not assigned yet, such as a bridge's before the bridge is up
/// at boot: for 120 s from the first try, pausing 1 s, then twice as long each time, up to 16 s.
const PROXY_BIND: Backoff = Backoff {
    wait: Duration::from_secs(120),
    first: Duration::from_secs(1),
    max: Duration::from_secs(16),
};

/// The server: the verb table, the token store, one handler per verb and the GET routes.
pub struct Server {
    pub(crate) verbs: &'static [Verb],
    pub(crate) tokens: TokenStore,
    pub(crate) handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    /// Each GET route: its path, the verb whose scope and tool list admit a caller to it, and its handler.
    gets: Vec<(&'static str, &'static str, Arc<dyn GetHandler>)>,
    /// Whether the node only looks: it reads and answers, and changes nothing. Every answer of such a node says so.
    pub(crate) only_looks: bool,
    /// Where the line for an answer of "internal" goes: the process's standard error, unless a test collects it.
    pub(crate) log: Arc<dyn Fn(&str) + Send + Sync>,
    /// The node's way to approve a plan from the admin token's terminal ([`Server::terminal_approval`]).
    approver: Option<Arc<dyn TerminalApprover>>,
    terminal_timeout: Duration,
    /// Built by [`Server::router`] from the two above.
    pub(crate) terminal: Option<crate::elicit::Terminal>,
    /// The authorization server's tokens and the URL of this server's protected resource metadata ([`Server::oauth`]).
    oauth: Option<(Arc<dyn IssuedTokens>, String)>,
}

/// Answers one GET route with a body. The server runs it on a blocking thread, after the bearer check.
pub trait GetHandler: Send + Sync {
    /// The reply to a GET with the query string `query`, by `caller`. `CallError::Invalid` is answered 400.
    fn get(&self, query: Option<&str>, caller: &Caller) -> Result<GetReply, CallError>;
}

/// A GET route's reply: its content type and its body.
pub struct GetReply {
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Server {
    /// A server over `verbs`, authenticating with `tokens`, with no handler yet.
    ///
    /// # Panics
    ///
    /// Panics when a verb writes but is open to viewers: a write needs a scope of `doctor` or higher, and a table that
    /// says otherwise is a wiring error, found at start.
    pub fn new(verbs: &'static [Verb], tokens: TokenStore) -> Self {
        for verb in verbs {
            assert!(
                !(verb.writes && verb.min_scope < Scope::Doctor),
                "the verb {} writes but is open to viewers; a write needs doctor or higher",
                verb.name
            );
        }
        Server {
            verbs,
            tokens,
            handlers: HashMap::new(),
            gets: Vec::new(),
            only_looks: false,
            log: Arc::new(|line| eprintln!("{line}")),
            approver: None,
            terminal_timeout: crate::elicit::DEFAULT_TIMEOUT,
            terminal: None,
            oauth: None,
        }
    }

    /// Accepts the tokens an OAuth authorization server issued for this server, besides the stored ones, and names
    /// `resource_metadata`, the URL of this server's protected resource metadata (RFC 9728), in every `401`:
    /// `WWW-Authenticate: Bearer resource_metadata="<url>", scope="viewer"`, so an MCP client given only this server's URL
    /// finds where to ask. A caller with an issued token refused a verb for its scope gets `403` with
    /// `error="insufficient_scope"` and the scope the verb needs, so its client can ask the owner again. A stored token is
    /// checked first and answered exactly as without this.
    pub fn oauth(mut self, issued: Arc<dyn IssuedTokens>, resource_metadata: String) -> Self {
        self.oauth = Some((issued, resource_metadata));
        self
    }

    /// The caller of a request: a stored token first, then one the authorization server issued.
    fn authenticate(&self, headers: &HeaderMap) -> Option<Caller> {
        let token = bearer(headers)?;
        self.tokens.lookup(token).or_else(|| {
            self.oauth
                .as_ref()
                .and_then(|(issued, _)| issued.lookup(token))
                .filter(Caller::is_issued)
        })
    }

    /// `401`, naming the protected resource metadata when there is an authorization server. A request that presented a
    /// token this server does not take (unknown, expired or revoked) is also told `invalid_token` (RFC 6750 3.1).
    fn unauthorized(&self, headers: &HeaderMap) -> Response {
        match &self.oauth {
            Some((_, metadata)) => {
                let error = if bearer(headers).is_some() {
                    "error=\"invalid_token\", "
                } else {
                    ""
                };
                challenge(
                    StatusCode::UNAUTHORIZED,
                    &format!("Bearer {error}resource_metadata=\"{metadata}\", scope=\"viewer\""),
                )
            }
            None => unauthorized(),
        }
    }

    /// `403` for a caller the verb `name` does not admit: with `insufficient_scope` and the verb's scope for an issued
    /// token, bare for a stored one.
    fn forbidden(&self, caller: &Caller, name: &str) -> Response {
        let (Some((_, metadata)), true) = (&self.oauth, caller.is_issued()) else {
            return StatusCode::FORBIDDEN.into_response();
        };
        let Some(verb) = self.verbs.iter().find(|verb| verb.name == name) else {
            return StatusCode::FORBIDDEN.into_response();
        };
        let mut scope = verb.min_scope.max(caller.scope).as_str().to_string();
        if let Some(tools) = &caller.tools {
            for tool in tools.iter().map(String::as_str).chain([verb.name]) {
                scope.push_str(" tool:");
                scope.push_str(tool);
            }
        }
        challenge(
            StatusCode::FORBIDDEN,
            &format!(
                "Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{metadata}\""
            ),
        )
    }

    /// Lets the node's one `admin` token approve a waiting plan from the terminal of a client that declared MCP
    /// elicitation (see [`crate::elicit`]). Every other caller, and every client that did not declare it, is answered as
    /// without this.
    pub fn terminal_approval(mut self, approver: Arc<dyn TerminalApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// How long the terminal's question waits for an answer before the plan is left waiting on its card.
    pub fn terminal_timeout(mut self, wait: Duration) -> Self {
        self.terminal_timeout = wait;
        self
    }

    /// Sends the line the door writes when it answers "internal" to `sink` instead of standard error. The caller of a
    /// tool is told only "internal: the tool failed"; the line is where the node's own log says which verb failed and
    /// why. It names the verb and a reason the server wrote, and never an argument of the call.
    pub fn log_to(mut self, sink: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.log = sink;
        self
    }

    /// Marks the node as one that only looks (or not). Every tool answer of such a node carries one sentence that says
    /// so, in its text and in its structured content, so a reader never takes what it is shown for something it can
    /// change here.
    pub fn only_looks(mut self, yes: bool) -> Self {
        self.only_looks = yes;
        self
    }

    /// Answers `verb` with `h`. A verb with no handler answers `not_found`.
    ///
    /// # Panics
    ///
    /// Panics when the table has no verb named `verb`: a handler for a verb that does not exist is a wiring error, and it
    /// is found at start, not at the first call.
    pub fn handle(mut self, verb: &'static str, h: Arc<dyn ToolHandler>) -> Self {
        assert!(
            self.verbs.iter().any(|v| v.name == verb),
            "the verb table has no verb named {verb}"
        );
        self.handlers.insert(verb, h);
        self
    }

    /// Answers `GET <path>` with `h`, behind the same bearer check and `Origin` refusal as `/mcp`, for a caller who may
    /// call the verb `tool`: the route gives the same reading that verb gives, so a token's scope and tool list admit it
    /// to the route exactly as to the verb. A request without a valid bearer is 401, one with an `Origin` is 403, and
    /// one from a token the verb does not admit is 403.
    ///
    /// # Panics
    ///
    /// Panics when `path` is `/mcp`, does not start with `/`, or already has a route, and when the table has no verb
    /// named `tool`: a wiring error, found at start.
    pub fn get_route(
        mut self,
        path: &'static str,
        tool: &'static str,
        h: Arc<dyn GetHandler>,
    ) -> Self {
        assert!(
            path.starts_with('/') && path != "/mcp" && self.gets.iter().all(|(p, _, _)| *p != path),
            "the GET route {path} is not a new path"
        );
        assert!(
            self.verbs.iter().any(|v| v.name == tool),
            "the verb table has no verb named {tool}"
        );
        self.gets.push((path, tool, h));
        self
    }

    /// The router: `POST /mcp`, with the body limit, and the GET routes. Any request with an `Origin` is refused before the
    /// body is read.
    pub fn router(mut self) -> Router {
        self.terminal = self
            .approver
            .take()
            .map(|approver| crate::elicit::Terminal::new(approver, self.terminal_timeout));
        let mut router = Router::new().route("/mcp", post(mcp));
        for (path, tool, handler) in std::mem::take(&mut self.gets) {
            router = router.route(
                path,
                get(
                    move |state: State<Arc<Server>>, uri: Uri, headers: HeaderMap| {
                        serve_get(state, handler, tool, uri, headers)
                    },
                ),
            );
        }
        router
            .fallback(not_found)
            .layer(middleware::from_fn(refuse_origin))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(Arc::new(self))
    }
}

/// Refuses a request that carries an `Origin` header, whatever its method or path, before any of its body is read.
async fn refuse_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn mcp(State(server): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(caller) = server.authenticate(&headers) else {
        return server.unauthorized(&headers);
    };
    let mirrors = Mirrors {
        version: mirrored(&headers, "mcp-protocol-version"),
        method: mirrored(&headers, "mcp-method"),
        name: mirrored(&headers, "mcp-name"),
    };
    // A client's answer to an elicitation request this server sent is not a request: it is handed to the call that asked,
    // and answered 202 (Streamable HTTP, "Sending Messages to the Server"). Any other body is dispatched as ever.
    if let Some(terminal) = &server.terminal
        && let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&body)
        && !object.contains_key("method")
        && (object.contains_key("result") || object.contains_key("error"))
        && let Some(id) = object.get("id").and_then(Value::as_str)
        && terminal.deliver(id, &caller.token_id, Value::Object(object.clone()))
    {
        return StatusCode::ACCEPTED.into_response();
    }
    match jsonrpc::dispatch(&server, &caller, &mirrors, &body).await {
        Ok(Some(answer)) if answer.stream.is_some() => sse(answer),
        Ok(Some(answer)) => (answer.status, Json(answer.body)).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(CallError::Forbidden { verb }) => server.forbidden(&caller, &verb),
        Err(CallError::Unauthenticated) => server.unauthorized(&headers),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// An answer that streams: `200` with `text/event-stream`. The first event carries an id and an empty `data`, to prime the
/// client's reconnect (the transport's SHOULD), then each message is one `data` event, and the stream ends after the last.
fn sse(answer: jsonrpc::Answer) -> Response {
    let Some(messages) = answer.stream else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let prime = tokio_stream::once(Ok::<_, io::Error>(Bytes::from_static(b"id: 0\ndata:\n\n")));
    let events = tokio_stream::wrappers::ReceiverStream::new(messages)
        .map(|message| Ok::<_, io::Error>(Bytes::from(format!("data: {message}\n\n"))));
    (
        answer.status,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        axum::body::Body::from_stream(tokio_stream::StreamExt::chain(prime, events)),
    )
        .into_response()
}

async fn serve_get(
    State(server): State<Arc<Server>>,
    handler: Arc<dyn GetHandler>,
    tool: &'static str,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let Some(caller) = server.authenticate(&headers) else {
        return server.unauthorized(&headers);
    };
    // The route is its verb's reading over GET: a token the verb does not admit, by scope or by tool list, is refused
    // here as a `tools/call` of the verb is.
    let admitted = server
        .verbs
        .iter()
        .find(|verb| verb.name == tool)
        .is_some_and(|verb| may_call(caller.scope, verb) && caller.allows_tool(verb.name));
    if !admitted {
        return server.forbidden(&caller, tool);
    }
    let query = uri.query().map(str::to_string);
    let outcome = tokio::task::spawn_blocking(move || handler.get(query.as_deref(), &caller)).await;
    match outcome {
        Ok(Ok(reply)) => ([(header::CONTENT_TYPE, reply.content_type)], reply.body).into_response(),
        Ok(Err(CallError::Invalid(_))) => StatusCode::BAD_REQUEST.into_response(),
        Ok(Err(CallError::Forbidden { .. })) => StatusCode::FORBIDDEN.into_response(),
        Ok(Err(CallError::Unauthenticated)) => server.unauthorized(&headers),
        Ok(Err(CallError::NotFound)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

/// `status` with one `WWW-Authenticate` challenge. A value that is no header falls back to the bare scheme.
fn challenge(status: StatusCode, value: &str) -> Response {
    let value = header::HeaderValue::from_str(value)
        .unwrap_or_else(|_| header::HeaderValue::from_static("Bearer"));
    (status, [(header::WWW_AUTHENTICATE, value)]).into_response()
}

/// The header `name` of a request, which mirrors a field of its body. A header sent twice has no one value.
fn mirrored<'a>(headers: &'a HeaderMap, name: &str) -> Mirrored<'a> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Mirrored::Absent,
        (Some(only), None) => only.to_str().map_or(Mirrored::Unreadable, Mirrored::Value),
        (Some(_), Some(_)) => Mirrored::Unreadable,
    }
}

/// The token of a single `Authorization: Bearer` header. The scheme is matched without regard to case; a second
/// `Authorization` header makes the credential ambiguous and so absent.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let only = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = only.to_str().ok()?.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// Serves `router` on a unix socket at `path`, mode 0600, to peers of this process's uid only. An existing regular file,
/// directory or symlink at `path` is refused and left alone. A socket that answers is a live server and is refused; a
/// socket that refuses connections is stale and is replaced; any other connect error fails the start.
pub async fn serve_unix(router: Router, path: &Path) -> io::Result<()> {
    serve_unix_bound(router, path, || {}).await
}

/// [`serve_unix`], calling `bound` once: after this process holds the socket at `path`, and before it serves a request.
/// A start that is refused (a live server on the path, something other than a socket there) never calls it, so what
/// `bound` publishes is true of this process's own listener.
pub async fn serve_unix_bound(router: Router, path: &Path, bound: impl FnOnce()) -> io::Result<()> {
    clear_stale_socket(path)?;
    let (listener, uid) = bind_staged(path)?;
    bound();
    let listener = OwnerListener {
        listener,
        uid,
        root: false,
        permits: permits(),
    };
    axum::serve(listener, router).await
}

/// Serves `router` on a second unix socket at `path`, the services socket: mode 0600, bound as [`serve_unix`] binds
/// its own, and served to peers of this process's uid and to root. Incus connects to it, as root, for a service whose
/// device forwards the instance's loopback to it. The caller passes the MCP router alone, whose every route needs a
/// bearer token, so the socket gives its peer no authority. The directory that holds the socket, and every directory
/// above it, must be one no other user can change ([`services_dir_ok`]), so no other user can put a socket of its own
/// at the path while the server is down.
pub async fn serve_services(router: Router, path: &Path) -> io::Result<()> {
    services_path_ok(path).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    services_dir_ok(path, process_uid()?)?;
    clear_stale_socket(path)?;
    let (listener, uid) = bind_staged(path)?;
    let listener = OwnerListener {
        listener,
        uid,
        root: true,
        permits: permits(),
    };
    axum::serve(listener, router).await
}

fn clear_stale_socket(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the socket path holds something other than a socket",
        ));
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a server already listens on the socket path",
        )),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path),
        Err(err) => Err(err),
    }
}

/// Binds a unix socket that is never reachable at `path` with a mode wider than 0600. The socket is bound inside a fresh
/// directory of mode 0700 beside `path`, chmodded 0600 and renamed into place, so the rename stays on one filesystem.
/// Returns the listener and the uid that owns the socket, read from the socket file this call has just made.
fn bind_staged(path: &Path) -> io::Result<(UnixListener, u32)> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let staging = fresh_dir(parent)?;
    let bound = bind_in(&staging.join("sock"), path);
    // Once the socket is renamed away the staging directory is empty; after a failure it holds the staged socket. Either
    // way it is ours and goes now.
    let _ = fs::remove_dir_all(&staging);
    bound
}

fn bind_in(staged: &Path, path: &Path) -> io::Result<(UnixListener, u32)> {
    let listener = UnixListener::bind(staged)?;
    fs::set_permissions(staged, fs::Permissions::from_mode(0o600))?;
    let uid = fs::symlink_metadata(staged)?.uid();
    fs::rename(staged, path)?;
    Ok((listener, uid))
}

/// Serves `router` on one tailnet address. The address must pass [`tailnet_addr_ok`], and every peer must pass
/// [`peer_allowed`].
pub async fn serve_tailnet(router: Router, addr: SocketAddr) -> io::Result<()> {
    tailnet_addr_ok(&addr).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let listener = TcpListener::bind(addr).await?;
    serve_filtered(router, listener, peer_allowed).await
}

/// Serves `router` on one proxy address, to the single peer address `peer` only. The address must pass
/// [`proxy_addr_ok`]. A peer is compared as its IPv4 address when it arrives IPv4-mapped, and every other peer is closed at
/// accept with no bytes sent. The bearer rules are the same as on every listener.
///
/// An address that is not assigned yet is bound again for up to 120 s (see [`PROXY_BIND`]), each try logged on stderr, so a
/// server started at boot before its bridge is up waits for the bridge. Any other bind error fails at once.
pub async fn serve_proxy(router: Router, addr: SocketAddr, peer: IpAddr) -> io::Result<()> {
    proxy_addr_ok(&addr).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let mut waited = false;
    let listener = bind_when_assigned(
        PROXY_BIND,
        || TcpListener::bind(addr),
        |error: &io::Error, pause: Duration| {
            waited = true;
            eprintln!(
                "arca: the proxy address {addr} cannot be bound yet ({error}); trying again in {:.1} s",
                pause.as_secs_f64()
            );
        },
    )
    .await
    .map_err(|error| io::Error::new(error.kind(), format!("the proxy address {addr}: {error}")))?;
    if waited {
        eprintln!("arca: the proxy address {addr} is bound");
    }
    let peer = peer.to_canonical();
    serve_filtered(router, listener, move |ip: &IpAddr| {
        ip.to_canonical() == peer
    })
    .await
}

/// Serves `router` on one loopback address, for clients on the same machine. The address must pass
/// [`loopback_addr_ok`]. These rules hold on this listener, whatever routes `router` carries:
///
/// - only a loopback peer is served, and any other is closed at accept with no bytes sent;
/// - a request must name the listener by its own address and port or by `localhost` and that port (see [`host_named`]);
///   any other `Host` is `403` before a route, the bearer check or the body is reached. A page a browser loaded from a
///   foreign name that was then pointed at loopback (DNS rebinding) sends that foreign name as its `Host`, so it reaches
///   nothing here, with or without an `Origin`;
/// - a peer has [`LOOPBACK_HEAD_TIMEOUT`] to send each request's head, and is closed when it does not;
/// - the bearer and `Origin` rules of `router` are unchanged.
///
/// A bind that fails does not end this future, so it never stops the server's other listeners: any program of the
/// machine can hold the port first, and a node that stopped for that could be kept down by any local user. Each failed
/// try goes to `on_bind` as `Some(error)`, the bind is tried again after a pause (1 s, doubling up to 30 s) for as long as
/// the future is polled, and the try that succeeds goes to `on_bind` as `None`. Only an address the rule refuses is an
/// error.
pub async fn serve_loopback(
    router: Router,
    addr: SocketAddr,
    on_bind: impl Fn(Option<&io::Error>) + Send + 'static,
) -> io::Result<()> {
    serve_loopback_with(router, addr, on_bind, LOOPBACK_HEAD_TIMEOUT).await
}

/// [`serve_loopback`] with the head timeout given.
///
/// Test seam: a test passes a timeout short enough to wait for. Production code calls [`serve_loopback`].
#[doc(hidden)]
pub async fn serve_loopback_with(
    router: Router,
    addr: SocketAddr,
    on_bind: impl Fn(Option<&io::Error>) + Send + 'static,
    head_timeout: Duration,
) -> io::Result<()> {
    loopback_addr_ok(&addr).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let mut pause = LOOPBACK_BIND_FIRST;
    let listener = loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => break listener,
            Err(error) => {
                on_bind(Some(&error));
                tokio::time::sleep(pause).await;
                pause = pause.saturating_mul(2).min(LOOPBACK_BIND_MAX);
            }
        }
    };
    let hosts = Arc::new(loopback_hosts(&listener.local_addr()?));
    on_bind(None);
    let router = router.layer(middleware::from_fn_with_state(hosts, require_host));
    let mut listener = Filtered {
        listener,
        allow: |ip: &IpAddr| ip.to_canonical().is_loopback(),
        permits: permits(),
    };
    // `axum::serve` gives hyper no timer, so hyper's head timeout never runs under it. This loop is `axum::serve` with
    // one: each connection is served on its own task, and its permit returns when the connection is dropped.
    loop {
        let (io, _peer) = axum::serve::Listener::accept(&mut listener).await;
        let service = TowerToHyperService::new(router.clone());
        tokio::spawn(async move {
            // HTTP/1 only: nothing is read from the peer outside hyper's own HTTP/1 state machine, whose timer
            // covers the first byte too. With both versions on, the version is sniffed first, under no timer.
            let mut server = auto::Builder::new(TokioExecutor::new()).http1_only();
            server
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(head_timeout);
            // A connection that ends in an error is the peer's own: a timeout, a reset or a malformed request.
            let _ = server.serve_connection(TokioIo::new(io), service).await;
        });
    }
}

/// The two names a request may give the loopback listener at `addr`: the address and port as a `Host` header writes them
/// (`127.0.0.1:7471`, `[::1]:7471`), and `localhost` with the same port.
fn loopback_hosts(addr: &SocketAddr) -> [String; 2] {
    [addr.to_string(), format!("localhost:{}", addr.port())]
}

/// Refuses, before anything else runs, a request that does not name one of `hosts`.
async fn require_host(
    State(hosts): State<Arc<[String; 2]>>,
    request: Request,
    next: Next,
) -> Response {
    if !host_named(&hosts[..], request.headers(), request.uri()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// Whether a request names one of `hosts` and nothing else. Every `Host` header it carries must be one of them, and so
/// must the authority of its target when the target has one (an absolute target, or HTTP/2's `:authority`); a request
/// with neither names nobody. Names are compared without regard to ASCII case, and the port is part of the name: `Host:
/// localhost` alone, or another port, is not this listener.
fn host_named(hosts: &[String], headers: &HeaderMap, uri: &Uri) -> bool {
    let listed = |name: &str| hosts.iter().any(|host| host.eq_ignore_ascii_case(name));
    let mut named = false;
    for value in headers.get_all(header::HOST) {
        if !value.to_str().is_ok_and(listed) {
            return false;
        }
        named = true;
    }
    if let Some(authority) = uri.authority() {
        if !listed(authority.as_str()) {
            return false;
        }
        named = true;
    }
    named
}

/// How a bind is tried again: for `wait` from the first try, pausing `first`, then twice as long each time, up to `max`.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    wait: Duration,
    first: Duration,
    max: Duration,
}

/// Runs `bind` until it succeeds or fails with an error other than [`io::ErrorKind::AddrNotAvailable`], which it returns at
/// once. While the address is not assigned, `bind` is tried again after each pause of `backoff`, and each failure and the
/// pause after it go to `log` first. The last pause ends at the end of the wait, and a try then that still finds the address
/// not assigned fails, naming the wait.
async fn bind_when_assigned<L, F, Fut>(
    backoff: Backoff,
    mut bind: F,
    mut log: impl FnMut(&io::Error, Duration),
) -> io::Result<L>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<L>>,
{
    let started = tokio::time::Instant::now();
    let mut pause = backoff.first;
    loop {
        let error = match bind().await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => error,
            Err(error) => return Err(error),
        };
        let left = backoff.wait.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("not assigned within {:?}: {error}", backoff.wait),
            ));
        }
        let next = pause.min(left);
        log(&error, next);
        tokio::time::sleep(next).await;
        pause = pause.saturating_mul(2).min(backoff.max);
    }
}

/// Serves `router` on an already bound TCP listener, admitting only the peers `allow` accepts, and at most
/// [`MAX_CONNECTIONS`] at once. A refused peer's connection is dropped at accept, so it receives no bytes.
/// [`serve_tailnet`] is this with [`peer_allowed`] and the tailnet check, and [`serve_proxy`] is this with one exact peer.
///
/// Test seam: a test passes its own `allow` to reach the filter from loopback. Production code calls [`serve_tailnet`] or
/// [`serve_proxy`].
#[doc(hidden)]
pub async fn serve_filtered(
    router: Router,
    listener: TcpListener,
    allow: impl Fn(&IpAddr) -> bool + Send + Sync + 'static,
) -> io::Result<()> {
    let listener = Filtered {
        listener,
        allow,
        permits: permits(),
    };
    axum::serve(listener, router).await
}

/// The most connections a listener holds: a semaphore of [`MAX_CONNECTIONS`] permits, one per connection served.
fn permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONNECTIONS))
}

/// Waits for a free slot. The semaphore is never closed, so a permit always comes.
async fn take_permit(permits: &Arc<Semaphore>) -> OwnedSemaphorePermit {
    Arc::clone(permits)
        .acquire_owned()
        .await
        .expect("the connection cap is never closed")
}

/// An accepted connection and the permit that counts it against [`MAX_CONNECTIONS`]. The permit returns when the
/// connection is dropped.
struct Permitted<S> {
    io: S,
    _permit: OwnedSemaphorePermit,
}

impl<S: AsyncRead + Unpin> AsyncRead for Permitted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Permitted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// The listener behind [`serve_filtered`]: it accepts, drops a refused peer and returns only an admitted one.
struct Filtered<F> {
    listener: TcpListener,
    allow: F,
    permits: Arc<Semaphore>,
}

impl<F> axum::serve::Listener for Filtered<F>
where
    F: Fn(&IpAddr) -> bool + Send + Sync + 'static,
{
    type Io = Permitted<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = take_permit(&self.permits).await;
            match self.listener.accept().await {
                Ok((stream, peer)) if (self.allow)(&peer.ip()) => {
                    return (
                        Permitted {
                            io: stream,
                            _permit: permit,
                        },
                        peer,
                    );
                }
                Ok((stream, _)) => drop(stream),
                Err(_) => {
                    drop(permit);
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// The listener of a unix socket: it serves only a peer whose credentials carry `uid`, and root when `root` is set, at
/// most [`MAX_CONNECTIONS`] at once.
struct OwnerListener {
    listener: UnixListener,
    uid: u32,
    /// Whether root is served too: the services socket alone.
    root: bool,
    permits: Arc<Semaphore>,
}

impl axum::serve::Listener for OwnerListener {
    type Io = Permitted<UnixStream>;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = take_permit(&self.permits).await;
            match self.listener.accept().await {
                Ok((stream, peer))
                    if owner_admitted(
                        stream.peer_cred().ok().map(|cred| cred.uid()),
                        self.uid,
                        self.root,
                    ) =>
                {
                    return (
                        Permitted {
                            io: stream,
                            _permit: permit,
                        },
                        peer,
                    );
                }
                Ok((stream, _)) => drop(stream),
                Err(_) => {
                    drop(permit);
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Whether a peer whose credentials read `peer_uid` is served by a server of `server_uid`: the same uid, and root when
/// `root` says the listener serves it. Credentials that cannot be read admit nobody.
fn owner_admitted(peer_uid: Option<u32>, server_uid: u32, root: bool) -> bool {
    peer_uid == Some(server_uid) || (root && peer_uid == Some(0))
}

/// Whether `addr` may be the tailnet listener: a fixed, unreserved port, not a wildcard, and an address in the tailnet's
/// IPv4 range (100.64.0.0/10) or its IPv6 range (fd7a:115c:a1e0::/48).
pub fn tailnet_addr_ok(addr: &SocketAddr) -> Result<(), String> {
    let port = addr.port();
    if port == 0 {
        return Err("the tailnet port must be a fixed number".into());
    }
    if RESERVED_PORTS.contains(&port) {
        return Err(format!(
            "port {port} is reserved and never a tailnet listener"
        ));
    }
    if addr.ip().is_unspecified() {
        return Err("a wildcard address is never a tailnet listener".into());
    }
    if !in_tailnet(&addr.ip()) {
        return Err("the address is outside the tailnet ranges".into());
    }
    Ok(())
}

/// Whether `addr` may be the proxy listener: a fixed, unreserved port, not a wildcard, and an address outside the tailnet
/// ranges, IPv4-mapped tailnet addresses included. A tailnet address is the tailnet listener's, never the proxy's.
pub fn proxy_addr_ok(addr: &SocketAddr) -> Result<(), String> {
    let port = addr.port();
    if port == 0 {
        return Err("the proxy port must be a fixed number".into());
    }
    if RESERVED_PORTS.contains(&port) {
        return Err(format!(
            "port {port} is reserved and never a proxy listener"
        ));
    }
    let ip = addr.ip().to_canonical();
    if ip.is_unspecified() {
        return Err("a wildcard address is never a proxy listener".into());
    }
    if in_tailnet(&ip) {
        return Err("a tailnet address is never a proxy listener".into());
    }
    Ok(())
}

/// Whether `addr` may be the loopback listener: a fixed, unreserved port on a loopback address (`127.0.0.0/8` or `::1`).
/// A wildcard, a tailnet address, any other address of the machine and an IPv4-mapped IPv6 address are all refused: an
/// address another machine can reach is the tailnet listener's or the proxy listener's, each under its own rule.
pub fn loopback_addr_ok(addr: &SocketAddr) -> Result<(), String> {
    let port = addr.port();
    if port == 0 {
        return Err("the loopback port must be a fixed number".into());
    }
    if RESERVED_PORTS.contains(&port) {
        return Err(format!(
            "port {port} is reserved and never a loopback listener"
        ));
    }
    if !addr.ip().is_loopback() {
        return Err(
            "the address is not a loopback address: only 127.0.0.0/8 and ::1 are, and no other address is served here"
                .into(),
        );
    }
    Ok(())
}

/// Whether the directory of the services socket at `path`, and every directory above it, is one no user but `uid` and
/// root can change. The directory itself, read through any symlink, must belong to `uid` or to root and be writable by
/// its owner alone: anyone who could write to it could bind a socket of their own at the path while the server is
/// down. Each directory above it must belong to `uid` or to root too, and be writable by its owner alone or carry the
/// sticky bit, as `/tmp` does: anyone who could rename a directory on the way could put another in its place.
fn services_dir_ok(path: &Path, uid: u32) -> io::Result<()> {
    let refused = |dir: &Path, why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the services socket's directory, and every directory above it, must belong to the server's user or to root and be writable by its owner alone (a directory above it may be sticky instead): {} {why}",
                dir.display()
            ),
        )
    };
    let parent = fs::canonicalize(path.parent().unwrap_or(Path::new("/")))?;
    for (depth, dir) in parent.ancestors().enumerate() {
        let held = fs::metadata(dir)?;
        if !held.is_dir() {
            return Err(refused(dir, "is not a directory"));
        }
        if held.uid() != uid && held.uid() != 0 {
            return Err(refused(dir, "belongs to another user"));
        }
        let open = held.mode() & 0o022 != 0;
        let sticky = held.mode() & 0o1000 != 0;
        if open && (depth == 0 || !sticky) {
            return Err(refused(dir, "can be written to by a group or by others"));
        }
    }
    Ok(())
}

/// Whether `path` may be the services socket: an absolute path of at most 100 bytes, made of letters, digits, `/`, `.`,
/// `_` and `-`, with no `..` part and a file name at its end. The path is written into an Incus device, so it holds
/// nothing a device value could read as anything else.
pub fn services_path_ok(path: &Path) -> Result<(), String> {
    let Some(text) = path.to_str() else {
        return Err("the path is not text".into());
    };
    if !text.starts_with('/') {
        return Err("the path must be absolute".into());
    }
    if text.len() > 100 {
        return Err("the path is longer than 100 bytes".into());
    }
    if !text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        return Err("the path holds a character other than letters, digits, /, ., _ and -".into());
    }
    if text.ends_with('/')
        || text
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("the path must name a file, with no empty, . or .. part".into());
    }
    Ok(())
}

/// Whether a TCP peer at `ip` may talk to the tailnet listener. An IPv4-mapped IPv6 peer is judged as its IPv4 address.
pub fn peer_allowed(ip: &IpAddr) -> bool {
    in_tailnet(&ip.to_canonical())
}

fn in_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{Duration, Instant};

    use super::{Backoff, bind_when_assigned, owner_admitted};

    /// A backoff short enough for a test: 10 ms, doubling up to 40 ms, for 300 ms.
    const QUICK: Backoff = Backoff {
        wait: Duration::from_millis(300),
        first: Duration::from_millis(10),
        max: Duration::from_millis(40),
    };

    fn not_assigned() -> io::Error {
        io::Error::from(io::ErrorKind::AddrNotAvailable)
    }

    #[tokio::test]
    async fn a_bind_refused_while_its_address_is_not_assigned_is_tried_again_with_a_doubling_pause()
    {
        let mut tries = 0;
        let mut pauses = Vec::new();
        // A wait no slow run reaches, so every pause is the backoff's own.
        let long = Backoff {
            wait: Duration::from_secs(30),
            ..QUICK
        };
        let bound = bind_when_assigned(
            long,
            || {
                tries += 1;
                let now = tries;
                async move {
                    if now < 5 {
                        Err(not_assigned())
                    } else {
                        Ok(now)
                    }
                }
            },
            |error: &io::Error, pause| {
                assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
                pauses.push(pause);
            },
        )
        .await;
        assert_eq!(bound.ok(), Some(5), "the fifth try binds");
        let ms = Duration::from_millis;
        assert_eq!(
            pauses,
            [ms(10), ms(20), ms(40), ms(40)],
            "each pause is logged"
        );
    }

    #[tokio::test]
    async fn a_bind_whose_address_is_still_not_assigned_at_the_end_of_the_wait_fails_naming_the_wait()
     {
        let started = Instant::now();
        let mut pauses = Vec::new();
        let bound: io::Result<()> = bind_when_assigned(
            QUICK,
            || async { Err(not_assigned()) },
            |_: &io::Error, pause| {
                pauses.push(pause);
            },
        )
        .await;
        let error = bound.expect_err("the address never comes up");
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
        assert!(error.to_string().contains("not assigned within"), "{error}");
        assert!(
            started.elapsed() >= QUICK.wait,
            "it tried for the whole wait"
        );
        assert!(pauses.iter().all(|pause| *pause <= QUICK.max), "{pauses:?}");
        assert!(pauses.iter().sum::<Duration>() <= QUICK.wait, "{pauses:?}");
    }

    #[tokio::test]
    async fn any_other_bind_error_fails_at_once_with_no_pause() {
        for kind in [
            io::ErrorKind::AddrInUse,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
        ] {
            let mut tries = 0;
            let mut pauses = Vec::new();
            let bound: io::Result<()> = bind_when_assigned(
                QUICK,
                || {
                    tries += 1;
                    async move { Err(io::Error::from(kind)) }
                },
                |_: &io::Error, pause| pauses.push(pause),
            )
            .await;
            assert_eq!(bound.map_err(|e| e.kind()), Err(kind));
            assert_eq!((tries, pauses.len()), (1, 0), "{kind:?} is not tried again");
        }
    }

    #[test]
    fn a_peer_is_admitted_only_with_the_servers_own_uid() {
        assert!(owner_admitted(Some(1000), 1000, false));
        assert!(!owner_admitted(Some(1001), 1000, false));
        assert!(!owner_admitted(Some(0), 1000, false));
        // The services socket serves root too, and still no other user.
        assert!(owner_admitted(Some(1000), 1000, true));
        assert!(owner_admitted(Some(0), 1000, true));
        assert!(!owner_admitted(Some(1001), 1000, true));
        assert!(!owner_admitted(None, 1000, true));
        assert!(
            !owner_admitted(None, 1000, false),
            "unreadable credentials admit nobody"
        );
    }
}
