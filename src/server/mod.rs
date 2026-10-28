pub mod chat;
pub mod completions;
pub mod error;
pub mod health;
pub mod models;
pub mod turn;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::Config;
use crate::harness::Registry;
use crate::routing::Catalog;
use crate::session_cache::SessionCache;
use error::ApiError;

pub struct AppState {
    pub cfg: Arc<Config>,
    pub registry: Arc<Registry>,
    pub catalog: Arc<Catalog>,
    pub inflight: Arc<Semaphore>,
    pub session_cache: Option<Arc<SessionCache>>,
    pub started: Instant,
}

impl AppState {
    pub fn new(cfg: Arc<Config>, registry: Arc<Registry>) -> Arc<Self> {
        let catalog = Arc::new(Catalog::new(cfg.clone(), registry.clone()));
        let session_cache = if cfg.session_cache.enabled {
            Some(SessionCache::new(
                cfg.session_cache.clone(),
                registry.clone(),
            ))
        } else {
            None
        };
        Arc::new(Self {
            inflight: Arc::new(Semaphore::new(cfg.limits.max_inflight)),
            cfg,
            registry,
            catalog,
            session_cache,
            started: Instant::now(),
        })
    }

    pub fn fingerprint(&self) -> String {
        format!("locum-{}", env!("CARGO_PKG_VERSION"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let cors = {
        let origins = &state.cfg.server.cors_origins;
        let allow = if origins.iter().any(|o| o == "*") {
            AllowOrigin::any()
        } else {
            AllowOrigin::list(origins.iter().filter_map(|o| o.parse::<HeaderValue>().ok()))
        };
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::ACCEPT,
                HeaderName::from_static("x-api-key"),
                HeaderName::from_static("x-request-id"),
            ])
            .expose_headers([
                HeaderName::from_static("x-request-id"),
                HeaderName::from_static("x-locum-harness"),
                HeaderName::from_static("x-locum-usage"),
                HeaderName::from_static("x-locum-resolved-model"),
            ])
    };

    let api = Router::new()
        .route("/v1/models", get(models::list))
        .route("/v1/models/{id}", get(models::get_one))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/completions", post(completions::completions))
        // Some clients omit the /v1 prefix (Ollama-style base URLs).
        .route("/models", get(models::list))
        .route("/chat/completions", post(chat::chat_completions))
        .route("/completions", post(completions::completions))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .merge(api)
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/", get(health::root))
        .fallback(fallback)
        .layer(middleware::from_fn(request_id))
        .layer(DefaultBodyLimit::max(state.cfg.limits.max_body_bytes))
        .layer(cors)
        .with_state(state)
}

async fn fallback() -> Response {
    ApiError {
        status: StatusCode::NOT_FOUND,
        kind: "invalid_request_error",
        code: Some("not_found"),
        message: "unknown endpoint".into(),
        param: None,
    }
    .into_response()
}

async fn request_id(mut req: Request<Body>, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::now_v7().simple().to_string());
    req.extensions_mut().insert(RequestId(id.clone()));
    let mut res = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", v);
    }
    res
}

#[derive(Debug, Clone)]
pub struct RequestId(pub String);

async fn auth(State(state): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    let keys = &state.cfg.auth.keys;
    if keys.is_empty() {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .or_else(|| req.headers().get("x-api-key").and_then(|v| v.to_str().ok()))
        .map(str::trim);
    let ok =
        presented.is_some_and(|p| keys.iter().any(|k| k.as_bytes().ct_eq(p.as_bytes()).into()));
    if ok {
        next.run(req).await
    } else {
        ApiError::unauthorized().into_response()
    }
}
