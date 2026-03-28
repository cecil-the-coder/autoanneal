//! 3-gate critic deliberation pipeline.

use crate::llm::{self, LlmInvocation};
use crate::models::*;
use crate::prompts::critic_panel as prompts;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{info, warn};

use super::critic::CriticOutput;

/// Maximum diff length (in characters) sent to critics.
const MAX_DIFF_CHARS: usize = 50_000;

/// Run a multi-model critic panel using 3-gate deliberation.
///
/// `model_specs` is a list of `(provider_hint, model_id)` pairs parsed from
/// the `--critic-models` flag.  Each model becomes an independent critic
/// instance.  The three gates (WORTHWHILE, READY, VERDICT) are evaluated
/// in sequence; the pipeline short-circuits if any gate fails.
///
/// Returns the same [`CriticOutput`] type as `phases::critic::run` so the
/// orchestrator can treat both paths identically.
pub async fn run(
    clone_path: &Path,
    default_branch: &str,
    model_specs: &[(Option<String>, String)],
    budget: f64,
    context_window: u64,
) -> Result<CriticOutput> {
    info!(
        models = model_specs.len(),
        budget,
        "starting critic panel deliberation"
    );

    // ── Get diff ────────────────────────────────────────────────────
    let diff = get_diff(clone_path, default_branch).await?;
    if diff.trim().is_empty() {
        return Ok(CriticOutput {
            score: 0,
            verdict: "reject".to_string(),
            summary: "No changes found to review.".to_string(),
            cost_usd: 0.0,
            made_fixes: false,
            score_unverified: false,
        });
    }

    // ── Ensure at least 3 critics (cycle through models) ────────────
    let num_critics = 3.max(model_specs.len());
    let critics: Vec<(Option<String>, String)> = (0..num_critics)
        .map(|i| {
            let spec = &model_specs[i % model_specs.len()];
            (spec.0.clone(), spec.1.clone())
        })
        .collect();

    // ── Budget allocation ───────────────────────────────────────────
    let gate1_budget = budget * 0.18;
    let gate2_budget = budget * 0.18;
    let gate3_budget = budget * 0.14;
    // remaining 50% reserved for fix/research

    let mut total_cost = 0.0;

    // ── Gate 1: WORTHWHILE ──────────────────────────────────────────
    // Budget per critic per round: divide gate budget by critics × 2 (allows for rebuttal)
    let (g1_passed, g1_responses, g1_cost) =
        run_gate1(&diff, &critics, gate1_budget / (critics.len() as f64 * 2.0), context_window, clone_path)
            .await?;
    total_cost += g1_cost;

    let _g1_entries: Vec<CriticEntry> = g1_responses
        .iter()
        .enumerate()
        .map(|(i, (_resp, cost))| CriticEntry {
            model: critics[i].1.clone(),
            role_hint: format!("gate1_variant_{}", (b'A' + (i % 3) as u8) as char),
            cost_usd: *cost,
        })
        .collect();

    if !g1_passed {
        // Pick the lowest-confidence reasoning as summary
        let summary = g1_responses
            .iter()
            .filter(|(r, _)| !r.proceed)
            .min_by(|a, b| a.0.confidence.partial_cmp(&b.0.confidence).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(r, _)| r.reasoning.clone())
            .unwrap_or_else(|| "Gate 1 rejected: changes not worthwhile.".to_string());

        info!(cost = total_cost, "gate 1 rejected — aborting deliberation");
        return Ok(CriticOutput {
            score: 3,
            verdict: "reject".to_string(),
            summary,
            cost_usd: total_cost,
            made_fixes: false,
            score_unverified: false,
        });
    }
    info!(cost = g1_cost, "gate 1 passed");

    // ── Gate 2: READY ───────────────────────────────────────────────
    let (g2_passed, g2_responses, g2_issues, g2_cost) =
        run_gate2(&diff, &critics, gate2_budget / (critics.len() as f64 * 2.0), context_window, clone_path)
            .await?;
    total_cost += g2_cost;

    let _g2_entries: Vec<CriticEntry> = g2_responses
        .iter()
        .enumerate()
        .map(|(i, (_, cost))| CriticEntry {
            model: critics[i].1.clone(),
            role_hint: "gate2_review".to_string(),
            cost_usd: *cost,
        })
        .collect();

    if !g2_passed {
        let issue_summary = g2_issues
            .iter()
            .take(3)
            .map(|iss| format!("- {}: {}", iss.file, iss.description))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = format!(
            "Gate 2 rejected: implementation has blocking issues.\n{}",
            issue_summary
        );

        info!(cost = total_cost, issues = g2_issues.len(), "gate 2 rejected — aborting deliberation");
        return Ok(CriticOutput {
            score: 4,
            verdict: "reject".to_string(),
            summary,
            cost_usd: total_cost,
            made_fixes: false,
            score_unverified: false,
        });
    }
    info!(cost = g2_cost, issues = g2_issues.len(), "gate 2 passed");

    // ── Gate 3: VERDICT ─────────────────────────────────────────────
    let (g3_score, g3_summary, g3_responses, g3_cost) =
        run_gate3(&diff, &critics, gate3_budget / critics.len() as f64, context_window, clone_path)
            .await?;
    total_cost += g3_cost;

    let _g3_entries: Vec<CriticEntry> = g3_responses
        .iter()
        .enumerate()
        .map(|(i, (_, cost))| CriticEntry {
            model: critics[i].1.clone(),
            role_hint: "gate3_verdict".to_string(),
            cost_usd: *cost,
        })
        .collect();

    let verdict = if g3_score >= 6 {
        "approve"
    } else if g3_score >= 4 {
        "needs_work"
    } else {
        "reject"
    };

    info!(
        score = g3_score,
        verdict,
        cost = total_cost,
        "critic panel deliberation complete"
    );

    Ok(CriticOutput {
        score: g3_score,
        verdict: verdict.to_string(),
        summary: g3_summary,
        cost_usd: total_cost,
        made_fixes: false,
        score_unverified: false,
    })
}

// ── Gate 1: WORTHWHILE ──────────────────────────────────────────────────

async fn run_gate1(
    diff: &str,
    critics: &[(Option<String>, String)],
    budget_per_critic: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(bool, Vec<(WorthwhileResponse, f64)>, f64)> {
    let user_prompt = format!(
        "## Changes Under Review\n\n```\n{}\n```\n\nEvaluate whether this PR should exist.",
        diff
    );

    // Round 1: parallel invocations
    let mut set = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::gate1_system_prompt(i).to_string();
        let prompt = user_prompt.clone();
        let model = critics[i].1.clone();
        let provider = critics[i].0.clone();
        let wd = clone_path.to_path_buf();
        set.spawn(async move {
            invoke_critic::<WorthwhileResponse>(
                system,
                prompt,
                model,
                provider,
                budget_per_critic,
                context_window,
                &wd,
            )
            .await
        });
    }

    let mut responses: Vec<(WorthwhileResponse, f64)> = Vec::with_capacity(critics.len());
    let mut total_cost = 0.0;

    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok((resp, cost))) => {
                total_cost += cost;
                responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, "gate1 critic invocation failed, using default");
                responses.push((
                    WorthwhileResponse {
                        proceed: false,
                        confidence: 0.1,
                        reasoning: "(critic unavailable — defaulting to reject)".into(),
                    },
                    0.0,
                ));
            }
            Err(e) => {
                warn!(error = %e, "gate1 critic task panicked, using default");
                responses.push((
                    WorthwhileResponse {
                        proceed: false,
                        confidence: 0.1,
                        reasoning: "(critic unavailable — defaulting to reject)".into(),
                    },
                    0.0,
                ));
            }
        }
    }

    // Check for unanimous decision
    let proceed_count = responses.iter().filter(|(r, _)| r.proceed).count();
    let reject_count = responses.len() - proceed_count;

    if proceed_count == responses.len() || reject_count == responses.len() {
        let passed = proceed_count == responses.len();
        info!(
            passed,
            unanimous = true,
            proceed_count,
            reject_count,
            "gate1 round 1 — unanimous"
        );
        return Ok((passed, responses, total_cost));
    }

    // Split vote — run rebuttal round
    info!(
        proceed_count,
        reject_count,
        "gate1 round 1 — split, running rebuttal"
    );

    let peer_text = format_responses_for_rebuttal(&responses);
    let rebuttal_user = prompts::GATE1_REBUTTAL
        .replace("{peer_responses}", &peer_text)
        .replace("{research_findings}", "(not available)");

    let mut rebuttal_set: JoinSet<Result<(WorthwhileResponse, f64)>> = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::gate1_system_prompt(i).to_string();
        let prompt = rebuttal_user.clone();
        let model = critics[i].1.clone();
        let provider = critics[i].0.clone();
        let wd = clone_path.to_path_buf();
        rebuttal_set.spawn(async move {
            invoke_critic::<WorthwhileResponse>(
                system,
                prompt,
                model,
                provider,
                budget_per_critic,
                context_window,
                &wd,
            )
            .await
        });
    }

    let mut rebuttal_responses: Vec<(WorthwhileResponse, f64)> = Vec::new();

    while let Some(result) = rebuttal_set.join_next().await {
        match result {
            Ok(Ok((resp, cost))) => {
                total_cost += cost;
                rebuttal_responses.push((resp, cost));
            }
            _ => {
                // On failure, keep original vote
            }
        }
    }

    // Use rebuttal responses where available, fall back to round 1 for the rest.
    // JoinSet returns results in completion order, so we use all rebuttals we got
    // and fill remaining slots with original round 1 responses.
    let final_responses = if rebuttal_responses.len() >= responses.len() {
        rebuttal_responses
    } else {
        // Append original responses for missing rebuttal slots
        let mut merged = rebuttal_responses;
        let needed = responses.len() - merged.len();
        merged.extend(responses.iter().rev().take(needed).cloned());
        merged
    };

    let proceed_count = final_responses.iter().filter(|(r, _)| r.proceed).count();
    let passed = proceed_count > final_responses.len() / 2;

    info!(
        passed,
        proceed_count,
        total = final_responses.len(),
        "gate1 after rebuttal — majority vote"
    );

    Ok((passed, final_responses, total_cost))
}

// ── Gate 2: READY ───────────────────────────────────────────────────────

async fn run_gate2(
    diff: &str,
    critics: &[(Option<String>, String)],
    budget_per_critic: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(bool, Vec<(ReadyResponse, f64)>, Vec<CriticIssue>, f64)> {
    let user_prompt = format!(
        "## Changes Under Review\n\n```\n{}\n```\n\nReview the implementation quality.",
        diff
    );

    let mut set = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::GATE2_SYSTEM.to_string();
        let prompt = user_prompt.clone();
        let model = critics[i].1.clone();
        let provider = critics[i].0.clone();
        let wd = clone_path.to_path_buf();
        set.spawn(async move {
            invoke_critic::<ReadyResponse>(
                system,
                prompt,
                model,
                provider,
                budget_per_critic,
                context_window,
                &wd,
            )
            .await
        });
    }

    let mut responses: Vec<(ReadyResponse, f64)> = Vec::with_capacity(critics.len());
    let mut total_cost = 0.0;

    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok((resp, cost))) => {
                total_cost += cost;
                responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, "gate2 critic invocation failed, using default");
                responses.push((
                    ReadyResponse {
                        verdict: "needs_fix".to_string(),
                        issues: vec![],
                        reasoning: "(critic unavailable — defaulting to needs_fix)".into(),
                    },
                    0.0,
                ));
            }
            Err(e) => {
                warn!(error = %e, "gate2 critic task panicked, using default");
                responses.push((
                    ReadyResponse {
                        verdict: "needs_fix".to_string(),
                        issues: vec![],
                        reasoning: "(critic unavailable — defaulting to needs_fix)".into(),
                    },
                    0.0,
                ));
            }
        }
    }

    // Merge and dedup issues
    let mut all_issues: Vec<CriticIssue> = Vec::new();
    for (resp, _) in &responses {
        for issue in &resp.issues {
            let dominated = all_issues.iter().any(|existing| {
                existing.file == issue.file
                    && issue
                        .description
                        .chars()
                        .take(50)
                        .collect::<String>()
                        == existing
                            .description
                            .chars()
                            .take(50)
                            .collect::<String>()
            });
            if !dominated {
                all_issues.push(issue.clone());
            }
        }
    }

    // Count rejects
    let reject_count = responses
        .iter()
        .filter(|(r, _)| r.verdict == "reject")
        .count();
    let passed = reject_count < (critics.len() + 1) / 2; // strict majority to reject

    info!(
        passed,
        reject_count,
        issues = all_issues.len(),
        "gate2 complete"
    );

    Ok((passed, responses, all_issues, total_cost))
}

// ── Gate 3: VERDICT ─────────────────────────────────────────────────────

async fn run_gate3(
    diff: &str,
    critics: &[(Option<String>, String)],
    budget_per_critic: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(u32, String, Vec<(VerdictResponse, f64)>, f64)> {
    let user_prompt = format!(
        "## Changes Under Review\n\n```\n{}\n```\n\nProvide your final score.",
        diff
    );

    let mut set = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::GATE3_SYSTEM.to_string();
        let prompt = user_prompt.clone();
        let model = critics[i].1.clone();
        let provider = critics[i].0.clone();
        let wd = clone_path.to_path_buf();
        set.spawn(async move {
            invoke_critic::<VerdictResponse>(
                system,
                prompt,
                model,
                provider,
                budget_per_critic,
                context_window,
                &wd,
            )
            .await
        });
    }

    let mut responses: Vec<(VerdictResponse, f64)> = Vec::with_capacity(critics.len());
    let mut total_cost = 0.0;

    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok((resp, cost))) => {
                total_cost += cost;
                responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, "gate3 critic invocation failed, using default");
                responses.push((
                    VerdictResponse {
                        score: 5,
                        summary: "(critic unavailable)".into(),
                    },
                    0.0,
                ));
            }
            Err(e) => {
                warn!(error = %e, "gate3 critic task panicked, using default");
                responses.push((
                    VerdictResponse {
                        score: 5,
                        summary: "(critic unavailable)".into(),
                    },
                    0.0,
                ));
            }
        }
    }

    // Compute median score
    let mut scores: Vec<u32> = responses.iter().map(|(r, _)| r.score).collect();
    let med = median(&mut scores);

    // Pick summary from the critic whose score is closest to the median
    let summary = responses
        .iter()
        .min_by_key(|(r, _)| (r.score as i64 - med as i64).unsigned_abs())
        .map(|(r, _)| r.summary.clone())
        .unwrap_or_else(|| "No verdict available.".to_string());

    info!(
        median_score = med,
        num_critics = responses.len(),
        "gate3 complete"
    );

    Ok((med, summary, responses, total_cost))
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Invoke a single critic and parse its structured response.
async fn invoke_critic<T: serde::de::DeserializeOwned + Send + 'static>(
    system_prompt: String,
    user_prompt: String,
    model: String,
    provider_hint: Option<String>,
    budget: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(T, f64)> {
    let invocation = LlmInvocation {
        prompt: user_prompt,
        system_prompt: Some(system_prompt),
        model,
        max_budget_usd: budget,
        max_turns: 1,
        effort: "high",
        tools: "",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        context_window,
        provider_hint,
    };

    let response = llm::invoke::<T>(&invocation, Duration::from_secs(300))
        .await
        .context("critic invocation failed")?;

    if let Some(structured) = response.structured {
        return Ok((structured, response.cost_usd));
    }

    // Fallback: try to extract JSON from the text response
    if !response.text.is_empty() {
        if let Some(json_block) = llm::extract_json_block(&response.text) {
            let parsed: T = serde_json::from_str(json_block)
                .context("failed to parse JSON block from critic response")?;
            return Ok((parsed, response.cost_usd));
        }

        // Try parsing the whole text as JSON
        if let Ok(parsed) = serde_json::from_str::<T>(&response.text) {
            return Ok((parsed, response.cost_usd));
        }
    }

    anyhow::bail!("critic returned neither structured output nor parseable JSON")
}

/// Compute the median of a mutable slice of u32 values.
/// For even N, returns the lower-middle element (conservative).
fn median(scores: &mut [u32]) -> u32 {
    if scores.is_empty() {
        return 5; // safe default
    }
    scores.sort_unstable();
    let mid = (scores.len() - 1) / 2;
    scores[mid]
}

/// Format Gate 1 responses for the rebuttal prompt.
fn format_responses_for_rebuttal(responses: &[(WorthwhileResponse, f64)]) -> String {
    responses
        .iter()
        .enumerate()
        .map(|(i, (resp, _))| {
            format!(
                "Critic {}: proceed={}, confidence={:.2}, reasoning={}",
                i + 1,
                resp.proceed,
                resp.confidence,
                resp.reasoning
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Get the diff between the default branch and HEAD.
async fn get_diff(clone_path: &Path, default_branch: &str) -> Result<String> {
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", &format!("{default_branch}..HEAD")])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to run git diff for critic panel")?;

    let diff = String::from_utf8_lossy(&diff_output.stdout).to_string();
    Ok(llm::truncate_to_char_boundary(&diff, MAX_DIFF_CHARS))
}
