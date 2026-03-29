use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::metrics::Metrics;
use crate::scheduler::TriggerMessage;
use crate::state::{RunRecord, StateStore, TriggerReason};

#[derive(Clone)]
pub struct AppState {
    pub state_store: Arc<StateStore>,
    pub trigger_tx: tokio::sync::mpsc::UnboundedSender<TriggerMessage>,
    pub metrics: Option<Arc<Metrics>>,
    pub webhook_secret: String,
    /// Map of (repo full name -> repo config name) for webhook routing.
    pub repo_configs: Arc<Mutex<HashMap<String, String>>>,
    /// Per-repo cooldown tracking for webhooks.
    pub webhook_cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
    /// Webhook cooldown duration in seconds.
    pub webhook_cooldown_secs: u64,
    /// Bearer token for API authentication. None = no auth required.
    pub api_token: Option<String>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Unauthenticated endpoints (k8s probes, prometheus, webhooks)
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/webhook/github", post(crate::webhook::handle_github_webhook))
        .route("/api/v1/runs", get(list_runs))
        // Authenticated endpoint
        .route("/api/v1/trigger", post(trigger_run))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, "ready")
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    if let Some(m) = state.metrics.as_ref() {
        Response::builder()
            .header("content-type", "text/plain; version=0.0.4")
            .body(Body::from(m.render()))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("metrics not available"))
            .unwrap()
    }
}

async fn list_runs(
    State(state): State<AppState>,
) -> Json<Vec<RunRecord>> {
    Json(state.state_store.recent_runs())
}

#[derive(Deserialize)]
struct TriggerRequest {
    repo: String,
}

#[derive(Serialize)]
struct TriggerResponse {
    status: String,
    message: String,
}

async fn trigger_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TriggerRequest>,
) -> impl IntoResponse {
    // Check bearer token auth if configured
    if let Some(ref expected_token) = state.api_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(token) if token == expected_token => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(TriggerResponse {
                        status: "error".into(),
                        message: "unauthorized".into(),
                    }),
                );
            }
        }
    }


    let msg = TriggerMessage {
        repo_name: req.repo,
        reason: TriggerReason::Manual,
    };

    if state.trigger_tx.send(msg).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(TriggerResponse {
                status: "error".into(),
                message: "scheduler not available".into(),
            }),
        );
    }

    (
        StatusCode::OK,
        Json(TriggerResponse {
            status: "triggered".into(),
            message: "run scheduled".into(),
        }),
    )
}

pub async fn run_server(state: AppState, listen_addr: &str) -> anyhow::Result<()> {
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app_state() -> AppState {
        let (trigger_tx, _trigger_rx) = tokio::sync::mpsc::unbounded_channel();
        let metrics = Arc::new(crate::metrics::Metrics::new().unwrap());
        AppState {
            state_store: Arc::new(StateStore::new(100)),
            trigger_tx,
            metrics: Some(metrics),
            webhook_secret: String::new(),
            repo_configs: Arc::new(Mutex::new(HashMap::new())),
            webhook_cooldowns: Arc::new(Mutex::new(HashMap::new())),
            webhook_cooldown_secs: 120,
            api_token: None,
        }
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = create_router(test_app_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn test_ready_endpoint() {
        let app = create_router(test_app_state());
        let req = Request::builder()
            .uri("/ready")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ready");
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let app = create_router(test_app_state());
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(content_type.contains("text/plain"));

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains("autoanneal_runs_total"));
    }
}
