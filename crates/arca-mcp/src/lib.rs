//! The Arca MCP server: the verb table served as MCP tools over streamable HTTP, JSON responses only.
//!
//! It speaks two protocol versions on the one `POST /mcp` route. A request that declares `2026-07-28` is answered as
//! that version asks: no handshake, the version and the method mirrored in headers, `server/discover`. Any other request
//! is answered as `2025-11-25` always was, `initialize` included, so a client written for it needs no change. The
//! [`version`] module decides which, from the request alone.
//!
//! Every request is refused `403` when it carries an `Origin` header. Otherwise it must carry a bearer token of a stored
//! scope, or it is refused `401`. With an authorization server ([`Server::oauth`]) a bearer it issued is accepted too,
//! and the `401` names the protected resource metadata (RFC 9728) where a client finds that server. The server keeps
//! only SHA-256 digests of the stored tokens, read from `<id>.hash` files.
//! It listens on a unix socket (mode 0600) and, optionally, on one tailnet address, on one proxy address that serves a
//! single peer, and on a second unix socket for the node's own services.

pub mod auth;
pub mod elicit;
mod fresh;
pub mod http;
pub mod jsonrpc;
pub mod tools;
pub mod version;

pub use auth::{
    Caller, ISSUED_PREFIX, IssuedTokens, TokenStore, TokenStoreError, admin_token_ids, mint,
    mint_with_tools,
};
pub use elicit::{PendingPlan, TerminalApprover};
pub use http::{
    GetHandler, GetReply, Server, loopback_addr_ok, peer_allowed, proxy_addr_ok, serve_filtered,
    serve_loopback, serve_loopback_with, serve_proxy, serve_services, serve_tailnet, serve_unix,
    serve_unix_bound, services_path_ok, tailnet_addr_ok,
};
pub use tools::{CallError, ToolHandler};
pub use version::{PROTOCOL_VERSION, STATELESS_PROTOCOL_VERSION, SUPPORTED_VERSIONS};
