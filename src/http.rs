//! MCP streamable HTTP (server side): POST for client messages, GET for the
//! server-notification stream, DELETE to end a session.

use crate::server::Server;
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::post,
};
use serde_json::{Value, json};
use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::BroadcastStream, wrappers::ReceiverStream};

pub struct AppState {
    pub servers: HashMap<String, Arc<Server>>,
    pub token: String,
}

const SESSION: &str = "mcp-session-id";

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{server}/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .with_state(state)
}

fn deny(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"jsonrpc": "2.0", "id": null,
                          "error": {"code": -32000, "message": msg}})))
        .into_response()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn loopback_host(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Bearer token plus Host/Origin checks (blocks browser DNS rebinding).
#[allow(clippy::result_large_err)]
fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
) -> Result<Arc<Server>, Response> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !loopback_host(host) {
        return Err(deny(StatusCode::FORBIDDEN, "host not allowed"));
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        let authority = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .unwrap_or("");
        if !loopback_host(authority) {
            return Err(deny(StatusCode::FORBIDDEN, "origin not allowed"));
        }
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if !ct_eq(presented.trim().as_bytes(), state.token.as_bytes()) {
        return Err(deny(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"));
    }
    state
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| deny(StatusCode::NOT_FOUND, "unknown server"))
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"))
}

fn new_session_id() -> String {
    let mut b = [0u8; 16];
    getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn getrandom(buf: &mut [u8]) {
    // SAFETY: arc4random_buf fills the buffer and cannot fail on macOS.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

fn sse_event(v: &Value) -> Result<Event, Infallible> {
    Ok(Event::default().event("message").data(v.to_string()))
}

async fn post_mcp(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let server = match authorize(&state, &headers, &name) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return (StatusCode::BAD_REQUEST,
                Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})))
                .into_response();
        }
    };
    if !msg.is_object() {
        return deny(StatusCode::BAD_REQUEST, "JSON-RPC batches are not supported");
    }
    let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
    let is_request = msg.get("id").is_some_and(|i| !i.is_null()) && method.is_some();
    let session = headers
        .get(SESSION)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

    if !is_request {
        // Notifications and responses: nothing to send back.
        if method.is_some() {
            server.client_notification(session.as_deref().unwrap_or("-"), &msg);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    if method.as_deref() == Some("initialize") {
        let sid = new_session_id();
        let client = msg
            .pointer("/params/clientInfo/name")
            .and_then(Value::as_str);
        server.touch_session(&sid, client);
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let reply = server.initialize(&id).await;
        let mut resp = Json(reply).into_response();
        resp.headers_mut()
            .insert(SESSION, HeaderValue::from_str(&sid).unwrap());
        return resp;
    }

    // Unknown session ids (e.g. after a daemon restart) are adopted rather than rejected,
    // so clients carry on without re-initializing.
    let sid = session.unwrap_or_else(|| "-".to_string());
    server.touch_session(&sid, None);

    // Tool calls stream their reply as SSE with keep-alives: a plain JSON response gives
    // no signal when the client hangs up, so the upstream call could never be cancelled.
    let streamed = method.as_deref() == Some("tools/call")
        || msg.pointer("/params/_meta/progressToken").is_some();
    if streamed && accepts_sse(&headers) {
        let (tx, rx) = mpsc::channel::<Value>(64);
        let (ptx, mut prx) = mpsc::channel::<Value>(64);
        let fwd_tx = tx.clone();
        tokio::spawn(async move {
            while let Some(p) = prx.recv().await {
                if fwd_tx.send(p).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            tokio::select! {
                reply = server.request(&sid, msg, Some(ptx)) => { let _ = tx.send(reply).await; }
                // Client went away: dropping the request future cancels upstream.
                _ = tx.closed() => {}
            }
        });
        let stream = ReceiverStream::new(rx).map(|v| sse_event(&v));
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
            .into_response();
    }

    Json(server.request(&sid, msg, None).await).into_response()
}

async fn get_mcp(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let server = match authorize(&state, &headers, &name) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !accepts_sse(&headers) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let stream = BroadcastStream::new(server.broadcast.subscribe())
        .filter_map(|m| m.ok())
        .map(|v| sse_event(&v));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
        .into_response()
}

async fn delete_mcp(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let server = match authorize(&state, &headers, &name) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Some(sid) = headers.get(SESSION).and_then(|h| h.to_str().ok()) {
        server.end_session(sid);
    }
    StatusCode::OK.into_response()
}
