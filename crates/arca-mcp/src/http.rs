//! The HTTP surface: the `POST /mcp` route, the unix socket listener and the tailnet listener with its peer filter.
//!
//! A request carrying any `Origin` header is refused `403` before its token is read: a browser page is never a client.
//! The unix socket is created at mode 0600. The tailnet listener binds one address that lies in the tailnet ranges, and
//! every TCP peer is checked against the same ranges; a peer outside them is dropped before any byte is read or written.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arca_api::Verb;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::net::{TcpListener, TcpStream, UnixListener};

use crate::auth::TokenStore;
use crate::jsonrpc;
use crate::tools::{CallError, ToolHandler};

/// The most body bytes one request may carry.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// The ports a tailnet listener may never take: the ones other services of the node use.
const RESERVED_PORTS: [u16; 4] = [80, 443, 7443, 9443];
/// How long the tailnet listener waits after a failed accept before it tries again.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// The server: the verb table, the token store and one handler per verb.
pub struct Server {
    pub(crate) verbs: &'static [Verb],
    pub(crate) tokens: TokenStore,
    pub(crate) handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
}

impl Server {
    /// A server over `verbs`, authenticating with `tokens`, with no handler yet.
    pub fn new(verbs: &'static [Verb], tokens: TokenStore) -> Self {
        Server {
            verbs,
            tokens,
            handlers: HashMap::new(),
        }
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

    /// The router: `POST /mcp`, with the body limit.
    pub fn router(self) -> Router {
        Router::new()
            .route("/mcp", post(mcp))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(Arc::new(self))
    }
}

async fn mcp(State(server): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(caller) = bearer(&headers).and_then(|token| server.tokens.lookup(token)) else {
        return unauthorized();
    };
    match jsonrpc::dispatch(&server, &caller, &body).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(CallError::Forbidden { .. }) => StatusCode::FORBIDDEN.into_response(),
        Err(CallError::Unauthenticated) => unauthorized(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
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

/// Serves `router` on a unix socket at `path`, mode 0600. An existing regular file, directory or symlink at `path` is
/// refused and left alone. A socket that answers is a live server and is refused; a socket that does not answer is stale
/// and is replaced.
pub async fn serve_unix(router: Router, path: &Path) -> io::Result<()> {
    clear_stale_socket(path)?;
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a server already listens on the socket path",
        ));
    }
    fs::remove_file(path)
}

/// Serves `router` on one tailnet address. The address must pass [`tailnet_addr_ok`], and every peer must pass
/// [`peer_allowed`].
pub async fn serve_tailnet(router: Router, addr: SocketAddr) -> io::Result<()> {
    tailnet_addr_ok(&addr).map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let listener = TcpListener::bind(addr).await?;
    serve_filtered(router, listener, peer_allowed).await
}

/// Serves `router` on an already bound TCP listener, admitting only the peers `allow` accepts. A refused peer's connection
/// is dropped at accept, so it receives no bytes. [`serve_tailnet`] is this with [`peer_allowed`] and the tailnet check.
pub async fn serve_filtered(
    router: Router,
    listener: TcpListener,
    allow: fn(&IpAddr) -> bool,
) -> io::Result<()> {
    axum::serve(Filtered { listener, allow }, router).await
}

/// The listener behind [`serve_filtered`]: it accepts, drops a refused peer and returns only an admitted one.
struct Filtered {
    listener: TcpListener,
    allow: fn(&IpAddr) -> bool,
}

impl axum::serve::Listener for Filtered {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) if (self.allow)(&peer.ip()) => return (stream, peer),
                Ok((stream, _)) => drop(stream),
                Err(_) => tokio::time::sleep(ACCEPT_RETRY).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
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

/// Whether a TCP peer at `ip` may talk to the tailnet listener. An IPv4-mapped IPv6 peer is judged as its IPv4 address.
pub fn peer_allowed(ip: &IpAddr) -> bool {
    let judged = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(*ip, IpAddr::V4),
        IpAddr::V4(_) => *ip,
    };
    in_tailnet(&judged)
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
