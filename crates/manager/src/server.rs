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
