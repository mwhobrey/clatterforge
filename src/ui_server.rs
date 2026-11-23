use crate::http_access::AccessPolicy;
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use tower_http::services::ServeDir;

pub async fn run(assets: PathBuf, port: u16) -> Result<()> {
    run_with_options(
        assets,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
        AccessPolicy::default_local(),
    )
    .await
}

pub async fn run_with_options(
    assets: PathBuf,
    bind: IpAddr,
    port: u16,
    access: AccessPolicy,
) -> Result<()> {
    let assets = std::fs::canonicalize(assets).context("resolve compiled GUI assets")?;
    ensure!(
        assets.join("index.html").is_file(),
        "compiled GUI index.html missing"
    );
    let access = Arc::new(access);
    let app = Router::new()
        .route("/health", get(|| async { "conductor-ui" }))
        .fallback_service(ServeDir::new(assets))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let access = Arc::clone(&access);
            async move {
                let host = req
                    .headers()
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !access.allows_host(host, port) {
                    crate::log_warn!("ui_server", "rejected request with unexpected Host", {
                        "host": host,
                        "path": req.uri().path(),
                        "port": port,
                    });
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::empty())
                        .unwrap();
                }
                let path = req.uri().path().to_string();
                let resp = next.run(req).await;
                crate::log_debug!("ui_server.http", "served static request", {
                    "path": path,
                    "status": resp.status().as_u16(),
                });
                resp
            }
        }));
    let address = SocketAddr::new(bind, port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("bind GUI listener {address}"))?;
    crate::log_info!("ui_server", "compiled GUI server listening", {
        "bind": address.to_string(),
        "port": port,
        "log_level": crate::logging::active_level().as_str(),
    });
    axum::serve(listener, app)
        .await
        .context("serve compiled GUI")
}
