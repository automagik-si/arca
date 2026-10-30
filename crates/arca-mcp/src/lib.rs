//! The Arca MCP server: the verb table served as MCP tools over streamable HTTP, JSON responses only.
//!
//! Every request is refused `403` when it carries an `Origin` header. Otherwise it must carry a bearer token of a stored
//! scope, or it is refused `401`. The server keeps only SHA-256 digests of the tokens, read from `<id>.hash` files.
//! It listens on a unix socket (mode 0600) and, optionally, on one tailnet address and on one proxy address that serves a
//! single peer.

pub mod auth;
mod fresh;
pub mod http;
pub mod jsonrpc;
pub mod tools;

pub use auth::{Caller, TokenStore, TokenStoreError, mint};
pub use http::{
    Server, peer_allowed, proxy_addr_ok, serve_filtered, serve_proxy, serve_tailnet, serve_unix,
    tailnet_addr_ok,
};
pub use tools::{CallError, ToolHandler};

/// The MCP protocol version the server speaks.
pub const PROTOCOL_VERSION: &str = "2025-11-25";
