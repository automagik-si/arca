//! The owner approval page: an open view of the waiting plans, one passkey approval POST, and one registration POST.
//!
//! The page routes are mounted only when `approvalOrigin` is configured. They sit outside the MCP bearer check and outside
//! the MCP `Origin` refusal: GETs admit no `Origin` or the exact configured one, and POSTs admit only the exact configured
//! one, before any body is read. `/mcp` and every other MCP route keep refusing every `Origin`; `crates/arca-mcp/src/http.rs`
//! is unchanged, and this router is merged into its result in `cmd_serve`.
//!
//! `approvalOrigin` is the page origin and the WebAuthn rp id. It is not the host an approval statement names: the statement
//! host stays `approval_host(&cfg)`.

use std::sync::{Arc, Mutex};

use arca_approval::passkey::format_passkey_line;
use arca_mcp::CallError;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::handlers::{Clock, approve_plan, list_plans};
use crate::kernel::Kernel;

/// The page's own body cap. `arca-mcp`'s 64 KiB layer is private and does not reach merged routes.
const PAGE_BODY_BYTES: usize = 64 * 1024;

/// The one page state: the configured origin and its rp id, the kernel plans are read from and approved in, and the clock.
struct PageState {
    origin: String,
    rp_id: String,
    kernel: Arc<Mutex<Kernel>>,
    clock: Clock,
}

/// Builds the page router for `origin`. The caller has already validated `origin`; this splits its host for the rp id.
///
/// # Panics
///
/// Panics when `origin` is not the validated `https://` origin. `cmd_serve` only calls this after the config check.
pub fn router(origin: String, kernel: Arc<Mutex<Kernel>>, clock: Clock) -> Router {
    let rp_id = origin
        .strip_prefix("https://")
        .and_then(|authority| authority.split(':').next())
        .filter(|host| !host.is_empty())
        .expect("approvalOrigin is validated before the page router is built")
        .to_string();
    let state = Arc::new(PageState {
        origin,
        rp_id,
        kernel,
        clock,
    });
    Router::new()
        .route("/approve", get(page))
        .route("/approve/plans", get(plans))
        .route("/approve/approve", post(approve))
        .route("/approve/register", post(register))
        .layer(DefaultBodyLimit::max(PAGE_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            origin_rule,
        ))
        .with_state(state)
}

/// The page's Origin rule, before any body is read: a POST needs the exact configured Origin; a GET needs none or the exact
/// one. Anything else is 403.
async fn origin_rule(
    State(state): State<Arc<PageState>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let allowed = if request.method() == Method::POST {
        origin == Some(state.origin.as_str())
    } else {
        origin.is_none() || origin == Some(state.origin.as_str())
    };
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn page() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("approve_page.html"),
    )
        .into_response()
}

async fn plans(State(state): State<Arc<PageState>>) -> Response {
    let now = (state.clock)();
    let kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match list_plans(&kernel, now) {
        Ok(plans) => Json(plans).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn approve(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(args) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let now = (state.clock)();
    let mut kernel = match state.kernel.lock() {
        Ok(kernel) => kernel,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match approve_plan(&mut kernel, &args, now) {
        Ok(result) => Json(result).into_response(),
        Err(CallError::Refused { code, message }) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "refusal": code, "message": message })),
        )
            .into_response(),
        Err(CallError::Invalid(_)) => StatusCode::BAD_REQUEST.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn register(State(state): State<Arc<PageState>>, body: Bytes) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(credential_id) = value.get("credentialId").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(public_key) = value.get("publicKey").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if value.get("publicKeyAlgorithm").and_then(Value::as_i64) != Some(-7) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match format_passkey_line(&state.rp_id, &state.origin, credential_id, public_key) {
        Ok(line) => Json(json!({ "line": line })).into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
