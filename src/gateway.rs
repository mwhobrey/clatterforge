//! A dedicated, loopback-only reverse proxy whose only job is topology restriction:
//! it is the one thing a public-facing tunnel (Cloudflare or otherwise) is ever
//! pointed at, and it knows how to forward exactly one route -
//! `POST /api/edge/tools/call` - to the real Conductor's own loopback port. Every
//! other path or method gets a plain 404, with no distinction in the response body,
//! before anything resembling the rest of the real API is ever reached. This is a
//! narrow allow-list, not a deny-list: a request this gateway doesn't recognize is
//! refused outright, never forwarded with a warning.
//!
//! This process performs no authentication of its own - that is still entirely
//! `authenticate_edge`'s job on the real Conductor (`src/serve.rs`), which a proxied
//! request still has to pass. The gateway's only responsibility is making sure the
//! rest of the Conductor's HTTP API (session cookies, pairing, task board, policies,
//! ...) is never reachable through whatever public ingress points at this process,
//! regardless of authentication - a narrower network topology, not a second auth
//! layer, and not a replacement for the real one.

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use std::{net::SocketAddr, time::Duration};

#[derive(Clone)]
struct GatewayState {
    client: reqwest::Client,
    upstream: String,
}

pub async fn run(port: u16, upstream: String, timeout_ms: u64) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .context("build upstream HTTP client")?;
    let state = GatewayState {
        client,
        upstream: upstream.trim_end_matches('/').to_string(),
    };
    let upstream_log = state.upstream.clone();
    let app = Router::new()
        .route("/api/edge/tools/call", post(forward))
        .fallback(not_found)
        .with_state(state);
    // Always loopback - this is a loopback gateway by design, not a configurable
    // bind address. A tunnel daemon (e.g. cloudflared) running on this same host is
    // the only intended caller.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind loopback {addr}"))?;
    eprintln!(
        "dev-conductor edge-gateway: listening on {addr}, forwarding only POST /api/edge/tools/call to {upstream_log}"
    );
    axum::serve(listener, app)
        .await
        .context("edge gateway server")
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Forwards only the body and a minimal, explicit header allow-list
/// (`Authorization`, `Content-Type`, and the two Cloudflare Access service-token
/// headers if present) - never the whole inbound header set, which would let a
/// caller smuggle headers this gateway never inspected or intended to pass through.
async fn forward(State(state): State<GatewayState>, headers: HeaderMap, body: Bytes) -> Response {
    let url = format!("{}/api/edge/tools/call", state.upstream);
    let mut request = state.client.post(&url).body(body);
    for name in [header::AUTHORIZATION, header::CONTENT_TYPE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value.clone());
        }
    }
    for name in ["cf-access-client-id", "cf-access-client-secret"] {
        if let Some(value) = headers.get(name) {
            request = request.header(name, value.clone());
        }
    }
    match request.send().await {
        Ok(upstream_response) => {
            let status = upstream_response.status();
            let content_type = upstream_response
                .headers()
                .get(header::CONTENT_TYPE)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("application/json"));
            match upstream_response.bytes().await {
                Ok(bytes) => {
                    let mut response = Response::new(Body::from(bytes));
                    *response.status_mut() = status;
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, content_type);
                    response
                }
                Err(error) => (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to read upstream response: {error}"),
                )
                    .into_response(),
            }
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("could not reach upstream conductor: {error}"),
        )
            .into_response(),
    }
}
