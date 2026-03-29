use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::scheduler::TriggerMessage;
use crate::state::TriggerReason;

type HmacSha256 = Hmac<Sha256>;

pub async fn handle_github_webhook(
    State(state): State<crate::server::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Record received webhook
    if let Some(metrics) = state.metrics.as_ref() {
        metrics.webhooks_received.inc();
    }

    // Verify signature if secret is configured
    let webhook_secret = &state.webhook_secret;
    if !webhook_secret.is_empty() {
        if let Err(e) = verify_signature(webhook_secret, &headers, &body) {
            warn!(error = %e, "webhook signature verification failed");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // Parse event type
    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Parse payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "failed to parse webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Extract repo full name
    let repo_full_name = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or("");

    debug!(event = %event, repo = %repo_full_name, "received webhook");

    // Find matching repo entry
    let repo_name = state.repo_configs.lock().unwrap()
        .iter()
        .find(|(key, _)| *key == repo_full_name)
        .map(|(_, name)| name.clone());

    let Some(repo_name) = repo_name else {
        debug!(repo = %repo_full_name, "no matching repo configured");
        return StatusCode::OK;
    };

    // Check cooldown
    let cooldown_key = repo_name.clone();
    let should_trigger = {
        let cooldowns = state.webhook_cooldowns.lock().unwrap();
        match cooldowns.get(&cooldown_key) {
            Some(last) => {
                let elapsed = last.elapsed();
                elapsed.as_secs() >= 120 // 2-minute cooldown
            }
            None => true,
        }
    };

    if !should_trigger {
        debug!(repo = %repo_name, "webhook cooldown active, skipping");
        return StatusCode::OK;
    }

    // Update cooldown
    {
        let mut cooldowns = state.webhook_cooldowns.lock().unwrap();
        cooldowns.insert(cooldown_key, std::time::Instant::now());
    }

    // Determine if this event should trigger a run
    let trigger = should_trigger_for_event(&event, &payload);
    let Some(reason) = trigger else {
        return StatusCode::OK;
    };

    // Send trigger
    let msg = TriggerMessage {
        repo_name: repo_name.clone(),
        reason,
    };

    if state.trigger_tx.send(msg).is_err() {
        warn!("failed to send trigger: scheduler not available");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    if let Some(metrics) = state.metrics.as_ref() {
        metrics.webhooks_triggered.inc();
    }

    info!(event = %event, repo = %repo_name, "webhook triggered run");
    StatusCode::OK
}

fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-Hub-Signature-256 header")?;

    let signature = signature
        .strip_prefix("sha256=")
        .ok_or("invalid signature format")?;

    let expected = hex::decode(signature).map_err(|e| format!("invalid hex: {e}"))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(body);
    let computed = mac.finalize().into_bytes();

    if computed.as_slice() == expected.as_slice() {
        Ok(())
    } else {
        Err("signature mismatch".into())
    }
}

fn should_trigger_for_event(event: &str, payload: &serde_json::Value) -> Option<TriggerReason> {
    match event {
        "push" => {
            let ref_ = payload["ref"].as_str().unwrap_or("").to_string();
            Some(TriggerReason::Webhook {
                event: "push".into(),
                ref_or_id: Some(ref_),
            })
        }
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or("");
            if matches!(action, "opened" | "synchronize" | "reopened") {
                let pr_number = payload["number"].as_u64().map(|n| n.to_string());
                Some(TriggerReason::Webhook {
                    event: "pull_request".into(),
                    ref_or_id: pr_number,
                })
            } else {
                None
            }
        }
        "check_suite" | "status" => {
            // Only trigger on failure
            let conclusion = payload["conclusion"].as_str()
                .or_else(|| payload["state"].as_str())
                .unwrap_or("");
            if conclusion == "failure" {
                Some(TriggerReason::Webhook {
                    event: event.into(),
                    ref_or_id: None,
                })
            } else {
                None
            }
        }
        "issues" => {
            let action = payload["action"].as_str().unwrap_or("");
            if matches!(action, "opened" | "labeled") {
                let issue_number = payload["issue"]["number"].as_u64().map(|n| n.to_string());
                Some(TriggerReason::Webhook {
                    event: "issues".into(),
                    ref_or_id: issue_number,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}
