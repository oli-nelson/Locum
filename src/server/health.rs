use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use super::AppState;

pub async fn root() -> impl IntoResponse {
    Json(json!({
        "name": "locum",
        "version": env!("CARGO_PKG_VERSION"),
        "openai_base_url": "/v1",
    }))
}

pub async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "uptime_s": state.started.elapsed().as_secs(),
        "inflight_available": state.inflight.available_permits(),
    }))
}

pub async fn readyz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut ready = false;
    let mut report = serde_json::Map::new();
    for (kind, h) in state.registry.iter() {
        let r = tokio::time::timeout(std::time::Duration::from_secs(20), h.probe()).await;
        let (ok, detail) = match r {
            Ok(Ok(info)) => (
                info.executable.is_some() && info.authenticated != Some(false),
                json!({
                    "executable": info.executable,
                    "version": info.version,
                    "authenticated": info.authenticated,
                    "detail": info.detail,
                }),
            ),
            Ok(Err(e)) => (false, json!({ "error": e.message })),
            Err(_) => (false, json!({ "error": "probe timed out" })),
        };
        ready |= ok;
        report.insert(kind.to_string(), detail);
    }
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "ready": ready, "harnesses": report })))
}
