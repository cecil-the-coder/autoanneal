use crate::llm::{self, truncate_to_char_boundary, LlmInvocation};
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
#[allow(dead_code)]
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
    /// True when fixes were applied but re-review was skipped (budget exhausted
    /// or re-review failed). Callers should treat the score as a lower bound
    /// and may enforce a stricter threshold.
    pub score_unverified: bool,
    /// The initial review summary before fixes were applied (for strikethrough in PR body).
    pub initial_summary: Option<String>,
    /// The initial score before fixes were applied.
    pub initial_score: Option<u32>,
}

pub async fn run(
    clone_path: &Path,
    default_branch: &str,
    model: &str,
    budget: f64,
    context_window: u64,
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
            score_unverified: false,
            initial_summary: None,
            initial_score: None,
        });
    }

    let prompt = CRITIC_PROMPT.replace("{diff}", &diff);
    let invocation = LlmInvocation {
        prompt,
        system_prompt: Some(critic_system_prompt()),
        model: model.to_string(),
        max_budget_usd: remaining_budget * 0.40,
        max_turns: 30,
        effort: "high",
        tools: "",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: None,
        ci_context: None,
        exa_max_searches: 0,
    };

    let response = llm::invoke::<CriticResult>(&invocation, Duration::from_secs(600)).await?;
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
        && (remaining_budget - total_cost) > budget * 0.15;

    if !should_fix {
        return Ok(CriticOutput {
            score: initial_review.score,
            verdict: initial_review.verdict,
            summary: initial_review.summary,
            cost_usd: total_cost,
            made_fixes: false,
            score_unverified: false,
            initial_summary: None,
            initial_score: None,
        });
    }

    info!("critic found fixable issues, attempting improvements");

    let fix_prompt = CRITIC_FIX_PROMPT
        .replace("{review_summary}", &initial_review.summary)
        .replace("{score}", &initial_review.score.to_string())
        .replace("{diff}", &diff);

    let fix_invocation = LlmInvocation {
        prompt: fix_prompt,
        system_prompt: Some(critic_fix_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget * 0.35,
        max_turns: 50,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: None,
        ci_context: None,
        exa_max_searches: 0,
    };

    let fix_response = llm::invoke::<serde_json::Value>(&fix_invocation, Duration::from_secs(600)).await;

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
                let add_succeeded = tokio::process::Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(clone_path)
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                let commit_succeeded = add_succeeded
                    && tokio::process::Command::new("git")
                        .args(["commit", "-m", "autoanneal: address review feedback"])
                        .current_dir(clone_path)
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false);

                if !commit_succeeded {
                    warn!("critic: git add/commit failed, skipping re-review");
                    return Ok(CriticOutput {
                        score: initial_review.score,
                        verdict: initial_review.verdict,
                        summary: initial_review.summary,
                        cost_usd: total_cost,
                        made_fixes: false,
                        score_unverified: false,
                        initial_summary: None,
                        initial_score: None,
                    });
                }

                info!("critic committed review fixes");

                // ─── Pass 3: Re-review ───────────────────────────────
                let new_diff = get_diff(clone_path, default_branch).await?;
                if !new_diff.trim().is_empty() && (remaining_budget - total_cost) > budget * 0.05 {
                    let re_prompt = CRITIC_PROMPT.replace("{diff}", &new_diff);
                    let re_invocation = LlmInvocation {
                        prompt: re_prompt,
                        system_prompt: Some(critic_system_prompt()),
                        model: model.to_string(),
                        max_budget_usd: budget * 0.25,
                        max_turns: 15,
                        effort: "high",
                        tools: "",
                        json_schema: None,
                        working_dir: clone_path.to_path_buf(),
                        context_window,
                        provider_hint: None,
                        max_tokens_per_turn: None,
                        ci_context: None,
        exa_max_searches: 0,
                    };

                    if let Ok(re_response) = llm::invoke::<CriticResult>(
                        &re_invocation,
                        Duration::from_secs(300),
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
                                summary: re_review.summary,
                                cost_usd: total_cost,
                                made_fixes: true,
                                score_unverified: false,
                                initial_summary: Some(initial_review.summary),
                                initial_score: Some(initial_review.score),
                            });
                        }
                    }
                }

                // Re-review failed or no budget — return original score without bump.
                // The score is unverified since fixes were applied without re-review.
                return Ok(CriticOutput {
                    score: initial_review.score,
                    verdict: initial_review.verdict.clone(),
                    summary: "Fixes applied but re-review skipped.".to_string(),
                    cost_usd: total_cost,
                    made_fixes: true,
                    score_unverified: true,
                    initial_summary: Some(initial_review.summary),
                    initial_score: Some(initial_review.score),
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
        score_unverified: false,
        initial_summary: None,
        initial_score: None,
    })
}

async fn get_diff(clone_path: &Path, default_branch: &str) -> Result<String> {
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", &format!("{default_branch}...HEAD")])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to run git diff for critic review")?;

    let diff = String::from_utf8_lossy(&diff_output.stdout).to_string();
    Ok(truncate_to_char_boundary(&diff, MAX_DIFF_CHARS))
}

