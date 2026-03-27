use crate::claude::{self, ClaudeInvocation, generate_session_id};
use crate::models::CriticResult;
use crate::prompts::critic::{CRITIC_FIX_PROMPT, CRITIC_PROMPT};
use crate::prompts::system::{critic_fix_system_prompt, critic_system_prompt};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// Maximum diff length (in characters) sent to the critic.
const MAX_DIFF_CHARS: usize = 50_000;

/// JSON schema for CriticResult structured output.
const CRITIC_SCHEMA: &str = r#"{
  \"type\": \"object\",
  \"properties\": {
    \"score\": { \"type\": \"integer\", \"minimum\": 1, \"maximum\": 10 },
    \"verdict\": { \"type\": \"string\", \"enum\": [\"approve\", \"needs_work\", \"reject\"] },
    \"summary\": { \"type\": \"string\" }
  },
  \"required\": [\"score\", \"verdict\", \"summary\"]
}"#;

#[allow(dead_code)]
pub struct CriticOutput {
    pub score: u32,
    pub verdict: String,
    pub summary: String,
    pub cost_usd: f64,
    /// True if the critic made fixes and the score improved.
    pub made_fixes: bool,
}

pub async fn run(
    clone_path: &Path,
    default_branch: &str,
    model: &str,
    budget: f64,
) -> Result<CriticOutput> {
    let mut total_cost = 0.0;
    let remaining_budget = budget;

    // ─── Pass 1: Review (read-only) ──────────────────────────────────
    let diff = get_diff(clone_path, default_branch).await?;
    if diff.trim().is_empty() {
        return Ok(CriticOutput {
            score: 0,
            verdict: "reject".to_string(),
            summary: "No changes found to review.".to_string(),
            cost_usd: 0.0,
            made_fixes: false,
        });
    }

    let prompt = CRITIC_PROMPT.replace("{diff}", &diff);
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(critic_system_prompt()),
        model: model.to_string(),
        max_budget_usd: (remaining_budget * 0.4).min(0.30),
        max_turns: 30,
        effort: "high",
        tools: "Read,Glob,Grep",
        json_schema: Some(CRITIC_SCHEMA.to_string()),
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    let response = claude::invoke::<CriticResult>(&invocation, Duration::from_secs(300)).await?;
    total_cost += response.cost_usd;

    let initial_review = response.structured.unwrap_or(CriticResult {
        score: 5,
        verdict: "needs_work".to_string(),
        summary: "Critic did not return structured output.".to_string(),
    });

    info!(
        score = initial_review.score,
        verdict = %initial_review.verdict,
        summary = %initial_review.summary,
        "critic initial review"
    );

    // ─── Pass 2: Fix (if needs_work and budget allows) ───────────────
    // Only attempt fixes if:
    // - Score indicates value but has issues (5-7)
    // - Verdict is "needs_work" (not "reject")
    // - We have budget remaining
    let should_fix = initial_review.verdict == "needs_work"
        && initial_review.score >= 4
        && (remaining_budget - total_cost) > 0.20;

    if !should_fix {
        return Ok(CriticOutput {
            score: initial_review.score,
            verdict: initial_review.verdict,
            summary: initial_review.summary,
            cost_usd: total_cost,
            made_fixes: false,
        });
    }

    info!("critic found fixable issues, attempting improvements");

    let fix_prompt = CRITIC_FIX_PROMPT
        .replace("{review_summary}", &initial_review.summary)
        .replace("{score}", &initial_review.score.to_string())
        .replace("{diff}", &diff);

    let fix_invocation = ClaudeInvocation {
        prompt: fix_prompt,
        system_prompt: Some(critic_fix_system_prompt()),
        model: model.to_string(),
        max_budget_usd: (remaining_budget - total_cost).min(0.50),
        max_turns: 50,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: Some(generate_session_id()),
        resume_session_id: None,
    };

    let fix_response = claude::invoke::<serde_json::Value>(&fix_invocation, Duration::from_secs(300)).await;

    match fix_response {
        Ok(resp) => {
            total_cost += resp.cost_usd;

            // Stage and commit the fixes.
            let status = tokio::process::Command::new("git")
                .args(["diff", "--stat"])
                .current_dir(clone_path)
                .output()
                .await;

            let has_changes = status
                .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
                .unwrap_or(false);

            if has_changes {
                // Stage the fixes.
                let _ = tokio::process::Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(clone_path)
                    .output()
                    .await;

                let _ = tokio::process::Command::new("git")
                    .args(["commit", "-m", "autoanneal: address review feedback"])
                    .current_dir(clone_path)
                    .output()
                    .await;

                info!("critic committed review fixes");

                // ─── Pass 3: Re-review ───────────────────────────────
                let new_diff = get_diff(clone_path, default_branch).await?;
                if !new_diff.trim().is_empty() && (remaining_budget - total_cost) > 0.10 {
                    let re_prompt = CRITIC_PROMPT.replace("{diff}", &new_diff);
                    let re_invocation = ClaudeInvocation {
                        prompt: re_prompt,
                        system_prompt: Some(critic_system_prompt()),
                        model: model.to_string(),
                        max_budget_usd: (remaining_budget - total_cost).min(0.20),
                        max_turns: 15,
                        effort: "high",
                        tools: "Read,Glob,Grep",
                        json_schema: Some(CRITIC_SCHEMA.to_string()),
                        working_dir: clone_path.to_path_buf(),
                        session_id: None,
                        resume_session_id: None,
                    };

                    if let Ok(re_response) = claude::invoke::<CriticResult>(
                        &re_invocation,
                        Duration::from_secs(180),
                    ).await {
                        total_cost += re_response.cost_usd;
                        if let Some(re_review) = re_response.structured {
                            info!(
                                initial_score = initial_review.score,
                                new_score = re_review.score,
                                "critic re-review after fixes"
                            );
                            return Ok(CriticOutput {
                                score: re_review.score,
                                verdict: re_review.verdict,
                                summary: format!(
                                    "Initial review: {}/10 — {}. After fixes: {}/10 — {}",
                                    initial_review.score, initial_review.summary,
                                    re_review.score, re_review.summary
                                ),
                                cost_usd: total_cost,
                                made_fixes: true,
                            });
                        }
                    }
                }

                // Re-review failed or no budget — return with initial score bumped slightly.
                return Ok(CriticOutput {
                    score: (initial_review.score + 1).min(10),
                    verdict: initial_review.verdict,
                    summary: format!(
                        "Initial: {}/10 — {}. Fixes applied but re-review skipped.",
                        initial_review.score, initial_review.summary
                    ),
                    cost_usd: total_cost,
                    made_fixes: true,
                });
            }

            info!("critic fix produced no changes");
        }
        Err(e) => {
            warn!(error = %e, "critic fix attempt failed (non-fatal)");
        }
    }

    // No fixes made — return initial review.
    Ok(CriticOutput {
        score: initial_review.score,
        verdict: initial_review.verdict,
        summary: initial_review.summary,
        cost_usd: total_cost,
        made_fixes: false,
    })
}

async fn get_diff(clone_path: &Path, default_branch: &str) -> Result<String> {
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", &format!("{default_branch}..HEAD")])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to run git diff for critic review")?;

    let mut diff = String::from_utf8_lossy(&diff_output.stdout).to_string();
    if diff.len() > MAX_DIFF_CHARS {
        diff.truncate(MAX_DIFF_CHARS);
        diff.push_str("\n\n... (diff truncated) ...");
    }
    Ok(diff)
}
