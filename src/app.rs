//! HTTP routes, authentication, concurrency limiting and request logging.

use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{rejection::BytesRejection, DefaultBodyLimit, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tracing::info;

use crate::{config::Config, error::ApiError, relay::relay, upstream::Ollama, validate, wol::Wol};

/// Large enough for a long agent conversation; anything bigger is refused with 413.
const MAX_BODY_BYTES: usize = 4 << 20;

struct AppState {
    config: Config,
    ollama: Ollama,
    wol: Option<Wol>,
    permits: Arc<Semaphore>,
    started: u64,
}

pub fn app(config: Config) -> Router {
    let ollama = Ollama::new(&config);
    let state = Arc::new(AppState {
        wol: config.wol.clone().map(|wol| Wol::new(wol, ollama.clone())),
        ollama,
        permits: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        started: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        config,
    });

    let v1 = Router::new()
        .route("/models", get(models))
        .route("/chat/completions", post(chat))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/health/ready", get(ready))
        .nest("/v1", v1)
        .fallback(|| async { ApiError::not_found() })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

// Logs method, path, status and latency only — never headers or bodies.
async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = Instant::now();
    let res = next.run(req).await;
    info!(%method, %path, status = res.status().as_u16(), latency_ms = start.elapsed().as_millis(), "request");
    res
}

async fn require_api_key(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    match token {
        Some(t) if bool::from(t.as_bytes().ct_eq(s.config.api_key.as_bytes())) => {
            next.run(req).await
        }
        _ => ApiError::unauthorized().into_response(),
    }
}

async fn ready(State(s): State<Arc<AppState>>) -> Response {
    match s.ollama.probe().await {
        Ok(()) => Json(json!({ "status": "ready" })).into_response(),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not_ready", "reason": reason })),
        )
            .into_response(),
    }
}

async fn models(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{ "id": s.config.virtual_model, "object": "model", "created": s.started, "owned_by": "opengate" }],
    }))
}

async fn chat(
    State(s): State<Arc<AppState>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = body.map_err(ApiError::from_body_rejection)?;
    let req: Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::invalid_request("invalid_json", "request body must be valid JSON")
    })?;
    let req = validate::chat_request(req, &s.config.virtual_model)?;
    if s.config.log_prompt_content {
        info!(messages = %req.messages(), "prompt content");
    }

    // No queue: a local GPU serves a couple of requests at once, and a fast,
    // clear 503 lets the client retry or fail over instead of piling up.
    let permit = s
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::overloaded())?;
    // Only authenticated, valid inference requests reach this point, so
    // nothing else can wake the Ollama host. Covers streaming and not.
    if let Some(wol) = &s.wol {
        wol.ensure_ready().await?;
    }
    let stream = req.stream;
    let res = s
        .ollama
        .chat(&req.into_upstream(&s.config.ollama_model))
        .await?;

    if stream {
        let frames = relay(
            res,
            s.config.virtual_model.clone(),
            s.config.stream_idle_timeout,
            permit,
        );
        return Ok(Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from_stream(frames))
            .expect("static headers are valid"));
    }

    let mut completion: Value = res.json().await.map_err(crate::upstream::from_reqwest)?;
    if !completion["choices"].is_array() {
        return Err(ApiError::upstream_invalid_response());
    }
    completion["model"] = s.config.virtual_model.as_str().into();
    drop(permit);
    Ok(Json(completion).into_response())
}
