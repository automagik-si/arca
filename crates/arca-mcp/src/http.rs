//! The HTTP surface: the `POST /mcp` route, the unix socket listener and the tailnet listener with its peer filter.
//!
//! A request carrying any `Origin` header is refused `403` before its body is read, whatever its method or path: a browser
//! page is never a client. The unix socket is bound inside a fresh private directory, chmodded 0600 and renamed into place,
//! so no other user ever reaches it at a wider mode, and it serves only peers of this process's own uid. The tailnet
//! listener binds one address that lies in the tailnet ranges, and every TCP peer is checked against the same ranges; a
//! peer outside them is dropped before any byte is read or written. Each listener serves at most [`MAX_CONNECTIONS`]
//! connections at once.

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

use arca_api::{Scope, Verb};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::TokenStore;
use crate::fresh::fresh_dir;
use crate::jsonrpc;
use crate::tools::{CallError, ToolHandler};

/// The most body bytes one request may carry.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// The most connections one listener serves at once. A connection past the cap waits in the kernel's backlog.
const MAX_CONNECTIONS: usize = 64;
/// The ports a tailnet listener may never take: the ones other services of the node use.
const RESERVED_PORTS: [u16; 4] = [80, 443, 7443, 9443];
/// How long a listener waits after a failed accept before it tries again.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// The server: the verb table, the token store and one handler per verb.
pub struct Server {
    pub(crate) verbs: &'static [Verb],
    pub(crate) tokens: TokenStore,
    pub(crate) handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
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

    /// The router: `POST /mcp`, with the body limit. Any request with an `Origin` is refused before the body is read.
    pub fn router(self) -> Router {
        Router::new()
            .route("/mcp", post(mcp))
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

/// Serves `router` on a unix socket at `path`, mode 0600, to peers of this process's uid only. An existing regular file,
/// directory or symlink at `path` is refused and left alone. A socket that answers is a live server and is refused; a
/// socket that refuses connections is stale and is replaced; any other connect error fails the start.
pub async fn serve_unix(router: Router, path: &Path) -> io::Result<()> {
    clear_stale_socket(path)?;
    let (listener, uid) = bind_staged(path)?;
    let listener = OwnerListener {
        listener,
        uid,
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

/// Serves `router` on an already bound TCP listener, admitting only the peers `allow` accepts, and at most
/// [`MAX_CONNECTIONS`] at once. A refused peer's connection is dropped at accept, so it receives no bytes.
/// [`serve_tailnet`] is this with [`peer_allowed`] and the tailnet check.
///
/// Test seam: a test passes its own `allow` to reach the filter from loopback. Production code calls [`serve_tailnet`].
#[doc(hidden)]
pub async fn serve_filtered(
    router: Router,
    listener: TcpListener,
    allow: fn(&IpAddr) -> bool,
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
struct Filtered {
    listener: TcpListener,
    allow: fn(&IpAddr) -> bool,
    permits: Arc<Semaphore>,
}

impl axum::serve::Listener for Filtered {
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

/// The listener of the unix socket: it serves only a peer whose credentials carry `uid`, at most [`MAX_CONNECTIONS`] at
/// once.
struct OwnerListener {
    listener: UnixListener,
    uid: u32,
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
                    if owner_admitted(stream.peer_cred().ok().map(|cred| cred.uid()), self.uid) =>
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

/// Whether a peer whose credentials read `peer_uid` is served by a server of `server_uid`. Credentials that cannot be read
/// admit nobody.
fn owner_admitted(peer_uid: Option<u32>, server_uid: u32) -> bool {
    peer_uid == Some(server_uid)
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

#[cfg(test)]
mod tests {
    use super::owner_admitted;

    #[test]
    fn a_peer_is_admitted_only_with_the_servers_own_uid() {
        assert!(owner_admitted(Some(1000), 1000));
        assert!(!owner_admitted(Some(1001), 1000));
        assert!(!owner_admitted(Some(0), 1000));
        assert!(
            !owner_admitted(None, 1000),
            "unreadable credentials admit nobody"
        );
    }
}
