//! The Arca API contracts: one verb table, the OpenRPC document generated from it, and the JSON Schema subset that checks
//! arguments and results against those schemas.

pub mod check;
pub mod compat;
pub mod openrpc;
pub mod refusals;
pub mod verbs;

pub use check::conforms;
pub use openrpc::openrpc;
pub use verbs::{Scope, Verb, VerbClass, may_call, verb, verbs};

/// The API version. It is the `info.version` of the OpenRPC document and the version of the frozen released schema.
pub const API_VERSION: &str = "0.1.0";
