//! 2-gate critic deliberation pipeline.

use crate::agent::bridge::parse_provider_model;
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

/// Run a multi-model critic panel using 2-gate deliberation.
///
/// `model_specs` is a list of `(provider_hint, model_id)` pairs parsed from
/// the `--critic-models` flag.  Each model becomes an independent critic
/// instance.  The two gates (WORTHWHILE, REVIEW) are evaluated in sequence;
/// the pipeline short-circuits if any gate fails.
///
/// When `skip_gate1` is true, Gate 1 is skipped (e.g., when re-reviewing
/// after a CI fix where worthwhileness was already established).
///
/// Returns the same [`CriticOutput`] type as `phases::critic::run` so the
/// orchestrator can treat both paths identically.
pub async fn run(
    clone_path: &Path,
    default_branch: &str,
    models: &[String],
    budget: f64,
    context_window: u64,
    skip_gate1: bool,
) -> Result<CriticOutput> {
    info!(
        models = models.len(),
        budget,
        skip_gate1,
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
    let num_critics = 3.max(models.len());
    let critics: Vec<String> = (0..num_critics)
        .map(|i| models[i % models.len()].clone())
        .collect();

    // ── Budget allocation ───────────────────────────────────────────
    let gate1_budget = budget * 0.25;
    let gate2_budget = budget * 0.75;

    let mut total_cost = 0.0;

    // ── Gate 1: WORTHWHILE ──────────────────────────────────────────
    // Gate 1 may run research internally during rebuttal (on split votes).
    // If it does, we reuse those findings for Gate 2 instead of running
    // research again.
    let mut gate1_research: Option<String> = None;

    if !skip_gate1 {
        let (g1_passed, g1_responses, g1_cost, g1_research) =
            run_gate1(&diff, &critics, gate1_budget / (critics.len() as f64 * 2.0), context_window, clone_path)
                .await?;
        total_cost += g1_cost;
        gate1_research = g1_research;

        if !g1_passed {
            let summary = g1_responses
                .iter()
                .filter(|(r, _)| r.verdict == "reject")
                .max_by(|a, b| {
                    let a_conf = if a.0.confidence.is_nan() { f64::NEG_INFINITY } else { a.0.confidence };
                    let b_conf = if b.0.confidence.is_nan() { f64::NEG_INFINITY } else { b.0.confidence };
                    a_conf.partial_cmp(&b_conf).unwrap_or(std::cmp::Ordering::Equal)
                })
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
    } else {
        info!("gate 1 skipped (skip_gate1=true)");
    };

    // ── Research agent ───────────────────────────────────────────────
    // If Gate 1 already ran research (during rebuttal), reuse those findings.
    // Otherwise run research now before Gate 2.
    let research_findings: Option<(String, f64)> = if let Some(findings) = gate1_research {
        info!(findings_len = findings.len(), "reusing research from gate 1 rebuttal");
        Some((findings, 0.0)) // cost already counted in gate 1
    } else if !skip_gate1 {
        // Gate 1 passed without rebuttal — run research now
        let research_budget = budget * 0.10;
        let findings = run_research(
            "Review the diff and investigate any potential issues with the changes.",
            &diff,
            &critics[0],
            research_budget,
            context_window,
            clone_path,
        )
        .await;

        if let Some((ref text, cost)) = findings {
            total_cost += cost;
            info!(cost, findings_len = text.len(), "research agent completed");
        }
        findings
    } else {
        None
    };

    // ── Gate 2: REVIEW ──────────────────────────────────────────────
    let (g2_passed, g2_responses, g2_issues, g2_median_score, g2_summary, g2_cost) =
        run_gate2(
            &diff,
            &critics,
            gate2_budget / (critics.len() as f64 * 2.0),
            context_window,
            clone_path,
            research_findings.as_ref().map(|(f, _)| f.as_str()),
        )
        .await?;
    total_cost += g2_cost;

    // Determine the dominant verdict from real responses only (exclude failed critics).
    let real_responses: Vec<&(ReadyResponse, f64)> = g2_responses
        .iter()
        .filter(|(r, _)| !r.reasoning.starts_with("(critic unavailable"))
        .collect();
    let approve_count = real_responses.iter().filter(|(r, _)| r.verdict == "approve").count();
    let needs_fix_count = real_responses.iter().filter(|(r, _)| r.verdict == "needs_fix").count();
    let reject_count = real_responses.iter().filter(|(r, _)| r.verdict == "reject").count();

    let (verdict, score) = if g2_passed && approve_count >= needs_fix_count && approve_count >= reject_count && g2_median_score >= 6 {
        // Approve: majority of real critics approves and score meets threshold
        ("approve".to_string(), g2_median_score)
    } else if !real_responses.is_empty() && reject_count > (real_responses.len() / 2) {
        // Reject: majority of real critics rejects
        ("reject".to_string(), g2_median_score)
    } else if needs_fix_count > 0 || !g2_passed {
        // Needs work: some critics want fixes
        let issue_summary = g2_issues
            .iter()
            .take(5)
            .map(|iss| format!("- {}: {}", iss.file, iss.description))
            .collect::<Vec<_>>()
            .join("\n");
        let summary_with_issues = if issue_summary.is_empty() {
            g2_summary.clone()
        } else {
            format!("{}\n\n## Issues\n{}", g2_summary, issue_summary)
        };

        info!(
            cost = total_cost,
            issues = g2_issues.len(),
            score = g2_median_score,
            "gate 2 needs_work — returning for fix loop"
        );
        return Ok(CriticOutput {
            score: g2_median_score,
            verdict: "needs_work".to_string(),
            summary: summary_with_issues,
            cost_usd: total_cost,
            made_fixes: false,
            score_unverified: false,
        });
    } else {
        // Fallback: use score to determine
        if g2_median_score >= 6 {
            ("approve".to_string(), g2_median_score)
        } else {
            ("needs_work".to_string(), g2_median_score)
        }
    };

    // Collect deductions from all critics for transparency.
    let all_deductions: Vec<String> = g2_responses
        .iter()
        .flat_map(|(r, _)| r.deductions.iter().cloned())
        .collect();
    let dedup_deductions: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        all_deductions.into_iter().filter(|d| seen.insert(d.clone())).collect()
    };

    let final_summary = if dedup_deductions.is_empty() {
        g2_summary
    } else {
        format!(
            "{}\n\n## Score Deductions\n{}",
            g2_summary,
            dedup_deductions.iter().map(|d| format!("- {d}")).collect::<Vec<_>>().join("\n")
        )
    };

    info!(
        score,
        verdict = %verdict,
        deductions = dedup_deductions.len(),
        cost = total_cost,
        "critic panel deliberation complete"
    );

    Ok(CriticOutput {
        score,
        verdict,
        summary: final_summary,
        cost_usd: total_cost,
        made_fixes: false,
        score_unverified: false,
    })
}

// ── Gate 1: WORTHWHILE ──────────────────────────────────────────────────

/// Returns (passed, responses, cost, research_findings).
/// research_findings is Some if research ran during a rebuttal round.
async fn run_gate1(
    diff: &str,
    critics: &[String],
    budget_per_critic: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(bool, Vec<(WorthwhileResponse, f64)>, f64, Option<String>)> {
    let user_prompt = format!(
        "## Changes Under Review\n\n```\n{}\n```\n\nEvaluate whether this PR should exist.",
        diff
    );

    // Round 1: parallel invocations
    let mut set = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::gate1_system_prompt(i).to_string();
        let prompt = user_prompt.clone();
        let model = critics[i].clone();
        let wd = clone_path.to_path_buf();
        set.spawn(async move {
            invoke_critic::<WorthwhileResponse>(
                system,
                prompt,
                model,
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
                let preview: String = resp.reasoning.chars().take(120).collect();
                info!(
                    gate = "worthwhile",
                    critic = responses.len() + 1,
                    verdict = %resp.verdict,
                    confidence = resp.confidence,
                    cost_usd = cost,
                    reasoning = %preview,
                    "gate1 critic responded"
                );
                total_cost += cost;
                responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, critic = responses.len() + 1, "gate1 critic failed");
                responses.push((
                    WorthwhileResponse {
                        verdict: "needs_work".to_string(),
                        confidence: 0.1,
                        reasoning: "(critic unavailable — defaulting to needs_work)".into(),
                    },
                    0.0,
                ));
            }
            Err(e) => {
                warn!(error = %e, critic = responses.len() + 1, "gate1 critic panicked");
                responses.push((
                    WorthwhileResponse {
                        verdict: "needs_work".to_string(),
                        confidence: 0.1,
                        reasoning: "(critic unavailable — defaulting to needs_work)".into(),
                    },
                    0.0,
                ));
            }
        }
    }

    // Filter out failed critics — only real responses count.
    let real_responses: Vec<&(WorthwhileResponse, f64)> = responses
        .iter()
        .filter(|(r, _)| !r.reasoning.starts_with("(critic unavailable"))
        .collect();
    let real_count = real_responses.len();

    info!(
        real_critics = real_count,
        total_critics = responses.len(),
        failed = responses.len() - real_count,
        "gate1: filtering out failed critics from voting"
    );

    // If no real responses, skip — we can't determine anything. Let the next run try.
    if real_responses.is_empty() {
        warn!("gate1: all critics failed, skipping (inconclusive)");
        anyhow::bail!("all Gate 1 critics failed — skipping review (will retry next run)");
    }

    let reject_count = real_responses.iter().filter(|(r, _)| r.verdict == "reject").count();
    let needs_work_count = real_responses.iter().filter(|(r, _)| r.verdict == "needs_work").count();
    let worthwhile_count = real_count - reject_count - needs_work_count;

    if reject_count == real_count {
        // Unanimous reject from real critics — abort immediately
        info!(reject_count, "gate1 round 1 — unanimous reject");
        return Ok((false, responses, total_cost, None));
    }
    if reject_count == 0 {
        // No rejects from real critics — proceed (even if some say needs_work)
        info!(worthwhile_count, needs_work_count, "gate1 round 1 — no rejects");
        return Ok((true, responses, total_cost, None));
    }

    // Mixed votes with some rejects — run research then rebuttal
    info!(
        reject_count,
        needs_work_count,
        worthwhile_count,
        "gate1 round 1 — mixed votes, running research then rebuttal"
    );

    // Run research agent to investigate disagreement before rebuttal
    let g1_text = format_all_responses_for_research(&responses);
    let research_budget = budget_per_critic * critics.len() as f64; // use one round's worth
    let research_result = run_research(
        &g1_text,
        diff,
        &critics[0],
        research_budget,
        context_window,
        clone_path,
    )
    .await;

    let research_text = if let Some((ref findings, cost)) = research_result {
        total_cost += cost;
        info!(cost, "gate1 research agent completed");
        format!("## Research Findings\n\n{findings}")
    } else {
        "(no research findings available)".to_string()
    };

    let peer_text = format_responses_for_rebuttal(&responses);
    let rebuttal_user = prompts::GATE1_REBUTTAL
        .replace("{peer_responses}", &peer_text)
        .replace("{research_findings}", &research_text);

    let mut rebuttal_set: JoinSet<Result<(WorthwhileResponse, f64)>> = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::gate1_system_prompt(i).to_string();
        let prompt = rebuttal_user.clone();
        let model = critics[i].clone();
        let wd = clone_path.to_path_buf();
        rebuttal_set.spawn(async move {
            invoke_critic::<WorthwhileResponse>(
                system,
                prompt,
                model,
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
                let preview: String = resp.reasoning.chars().take(120).collect();
                info!(
                    gate = "worthwhile_rebuttal",
                    critic = rebuttal_responses.len() + 1,
                    verdict = %resp.verdict,
                    confidence = resp.confidence,
                    cost_usd = cost,
                    reasoning = %preview,
                    "gate1 rebuttal responded"
                );
                total_cost += cost;
                rebuttal_responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, "gate1 rebuttal critic failed, keeping original vote");
            }
            Err(e) => {
                warn!(error = %e, "gate1 rebuttal task panicked, keeping original vote");
            }
        }
    }

    // Use rebuttal responses where available, fall back to round 1 for the rest.
    let final_responses = if rebuttal_responses.len() >= responses.len() {
        rebuttal_responses
    } else {
        let mut merged = rebuttal_responses;
        let needed = responses.len() - merged.len();
        merged.extend(responses.iter().rev().take(needed).cloned());
        merged
    };

    // Exclude failed critics from rebuttal vote too.
    let real_final: Vec<&(WorthwhileResponse, f64)> = final_responses
        .iter()
        .filter(|(r, _)| !r.reasoning.starts_with("(critic unavailable"))
        .collect();

    if real_final.is_empty() {
        anyhow::bail!("all Gate 1 rebuttal critics failed — skipping review (will retry next run)");
    }

    let reject_count = real_final.iter().filter(|(r, _)| r.verdict == "reject").count();
    let passed = reject_count <= real_final.len() / 2;

    info!(
        passed,
        reject_count,
        total = final_responses.len(),
        "gate1 after rebuttal — majority vote"
    );

    // Return research findings so run() can pass them to Gate 2
    let gate1_research = research_result.map(|(text, _)| text);
    Ok((passed, final_responses, total_cost, gate1_research))
}

// ── Gate 2: REVIEW ─────────────────────────────────────────────────────

async fn run_gate2(
    diff: &str,
    critics: &[String],
    budget_per_critic: f64,
    context_window: u64,
    clone_path: &Path,
    research_findings: Option<&str>,
) -> Result<(bool, Vec<(ReadyResponse, f64)>, Vec<CriticIssue>, u32, String, f64)> {
    let user_prompt = if let Some(findings) = research_findings {
        format!(
            "## Changes Under Review\n\n```\n{}\n```\n\n## Research Findings\n\nA research agent investigated the codebase and found:\n\n{}\n\nReview the implementation quality and provide your score.",
            diff, findings
        )
    } else {
        format!(
            "## Changes Under Review\n\n```\n{}\n```\n\nReview the implementation quality and provide your score.",
            diff
        )
    };

    let mut set = JoinSet::new();
    for i in 0..critics.len() {
        let system = prompts::GATE2_SYSTEM.to_string();
        let prompt = user_prompt.clone();
        let model = critics[i].clone();
        let wd = clone_path.to_path_buf();
        set.spawn(async move {
            invoke_critic::<ReadyResponse>(
                system,
                prompt,
                model,
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
                let preview: String = resp.reasoning.chars().take(120).collect();
                let deductions_preview = if resp.deductions.is_empty() {
                    "none".to_string()
                } else {
                    resp.deductions.join("; ")
                };
                info!(
                    gate = "review",
                    critic = responses.len() + 1,
                    verdict = %resp.verdict,
                    score = resp.score,
                    issues = resp.issues.len(),
                    deductions = %deductions_preview,
                    cost_usd = cost,
                    reasoning = %preview,
                    "gate2 critic responded"
                );
                total_cost += cost;
                responses.push((resp, cost));
            }
            Ok(Err(e)) => {
                warn!(error = %e, critic = responses.len() + 1, "gate2 critic failed");
                responses.push((
                    ReadyResponse {
                        verdict: "needs_fix".to_string(),
                        issues: vec![],
                        reasoning: "(critic unavailable — defaulting to needs_fix)".into(),
                        score: 5,
                        summary: "(critic unavailable)".into(),
                        deductions: vec!["Critic unavailable".into()],
                    },
                    0.0,
                ));
            }
            Err(e) => {
                warn!(error = %e, critic = responses.len() + 1, "gate2 critic panicked");
                responses.push((
                    ReadyResponse {
                        verdict: "needs_fix".to_string(),
                        issues: vec![],
                        reasoning: "(critic unavailable — defaulting to needs_fix)".into(),
                        score: 5,
                        summary: "(critic unavailable)".into(),
                        deductions: vec!["Critic unavailable".into()],
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

    // Separate real responses from failed critics.
    let real_responses: Vec<&(ReadyResponse, f64)> = responses
        .iter()
        .filter(|(r, _)| !r.reasoning.starts_with("(critic unavailable"))
        .collect();
    let real_count = real_responses.len();

    info!(
        real_critics = real_count,
        total_critics = responses.len(),
        failed = responses.len() - real_count,
        "gate2: filtering out failed critics from scoring"
    );

    // If no real responses, fail gracefully.
    if real_responses.is_empty() {
        warn!("gate2: all critics failed, skipping (inconclusive)");
        anyhow::bail!("all Gate 2 critics failed — skipping review (will retry next run)");
    }

    // Count rejects from real responses only.
    let reject_count = real_responses
        .iter()
        .filter(|(r, _)| r.verdict == "reject")
        .count();
    let passed = reject_count < (real_count + 1) / 2; // strict majority of real critics to reject

    // Compute median score from real responses only.
    let mut scores: Vec<u32> = real_responses.iter().map(|(r, _)| r.score).collect();
    let med = median(&mut scores);

    // Pick summary from the critic whose score is closest to the median
    let summary = responses
        .iter()
        .filter(|(r, _)| !r.summary.is_empty() && r.summary != "(critic unavailable)")
        .min_by_key(|(r, _)| (r.score as i64 - med as i64).unsigned_abs())
        .map(|(r, _)| r.summary.clone())
        .unwrap_or_else(|| {
            // Fallback: use reasoning from critic closest to median
            responses
                .iter()
                .min_by_key(|(r, _)| (r.score as i64 - med as i64).unsigned_abs())
                .map(|(r, _)| r.reasoning.chars().take(200).collect::<String>())
                .unwrap_or_else(|| "No review summary available.".to_string())
        });

    info!(
        passed,
        reject_count,
        median_score = med,
        issues = all_issues.len(),
        "gate2 complete"
    );

    Ok((passed, responses, all_issues, med, summary, total_cost))
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Invoke a single critic and parse its structured response.
async fn invoke_critic<T: serde::de::DeserializeOwned + Send + 'static>(
    system_prompt: String,
    user_prompt: String,
    model: String,
    budget: f64,
    context_window: u64,
    clone_path: &Path,
) -> Result<(T, f64)> {
    let (provider_hint, model_name) = parse_provider_model(&model);

    let invocation = LlmInvocation {
        prompt: user_prompt,
        system_prompt: Some(system_prompt),
        model: model_name,
        max_budget_usd: budget,
        max_turns: 1,
        effort: "high",
        tools: "",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        context_window,
        provider_hint,
        max_tokens_per_turn: Some(4096), // critics output small JSON, no need for 16K
    };

    let response = llm::invoke::<T>(&invocation, Duration::from_secs(300))
        .await
        .context("critic invocation failed")?;

    if let Some(structured) = response.structured {
        return Ok((structured, response.cost_usd));
    }

    // Fallback: try to extract JSON from the text response
    if !response.text.is_empty() {
        // Try extracting from code fence first, then the whole text
        let candidates = [
            llm::extract_json_block(&response.text).map(|s| s.to_string()),
            Some(response.text.clone()),
        ];

        for candidate in candidates.into_iter().flatten() {
            // Try direct parse
            if let Ok(parsed) = serde_json::from_str::<T>(&candidate) {
                return Ok((parsed, response.cost_usd));
            }

            // Sanitize common LLM JSON issues: unescaped newlines inside strings
            let sanitized = sanitize_json(&candidate);
            if let Ok(parsed) = serde_json::from_str::<T>(&sanitized) {
                return Ok((parsed, response.cost_usd));
            }
        }
    }

    // Log the full response for debugging parse failures
    warn!(
        text_len = response.text.len(),
        text = %response.text,
        "critic response could not be parsed as JSON"
    );
    let preview: String = response.text.chars().take(300).collect();
    anyhow::bail!("critic returned unparseable response: {preview}")
}

/// Sanitize common JSON issues from LLM output.
/// LLMs frequently produce JSON with unescaped newlines, tabs, or control
/// characters inside string values, which is invalid JSON.
fn sanitize_json(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_string = false;
    let mut prev_was_escape = false;

    for ch in input.chars() {
        if prev_was_escape {
            prev_was_escape = false;
            // Only valid JSON escapes: " \ / b f n r t u
            match ch {
                '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u' => {
                    result.push(ch);
                }
                _ => {
                    // Invalid escape like \_ or \g — double the backslash
                    // so it becomes a literal backslash in the JSON string.
                    result.push('\\');
                    result.push(ch);
                }
            }
            continue;
        }

        if ch == '\\' && in_string {
            result.push(ch);
            prev_was_escape = true;
            continue;
        }

        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            continue;
        }

        if in_string {
            match ch {
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                c if c.is_control() => {} // strip other control chars
                c => result.push(c),
            }
        } else {
            result.push(ch);
        }
    }

    result
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
                "Critic {}: verdict={}, confidence={:.2}, reasoning={}",
                i + 1,
                resp.verdict,
                resp.confidence,
                resp.reasoning
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Format Gate 1 responses for the research agent.
fn format_all_responses_for_research(responses: &[(WorthwhileResponse, f64)]) -> String {
    responses
        .iter()
        .enumerate()
        .map(|(i, (resp, _))| {
            format!(
                "Critic {} (verdict: {}): {}",
                i + 1,
                resp.verdict,
                resp.reasoning
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Run the research agent to investigate critic claims.
/// Returns findings as a string and cost, or None if no investigation needed.
async fn run_research(
    critic_responses: &str,
    diff: &str,
    model: &str,
    budget: f64,
    context_window: u64,
    clone_path: &Path,
) -> Option<(String, f64)> {
    let (provider_hint, model_name) = parse_provider_model(model);

    let user_prompt = prompts::RESEARCH_PROMPT
        .replace("{claims}", critic_responses)
        .replace("{diff}", diff);

    let invocation = LlmInvocation {
        prompt: user_prompt,
        system_prompt: Some(prompts::RESEARCH_SYSTEM.to_string()),
        model: model_name,
        max_budget_usd: budget,
        max_turns: 15,
        effort: "high",
        tools: "Read,Glob,Grep,Bash",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        context_window,
        provider_hint,
        max_tokens_per_turn: None,
    };

    match llm::invoke::<serde_json::Value>(&invocation, Duration::from_secs(120)).await {
        Ok(response) => {
            if response.text.trim().is_empty() {
                info!("research agent found nothing to investigate");
                None
            } else {
                Some((response.text, response.cost_usd))
            }
        }
        Err(e) => {
            warn!(error = %e, "research agent failed (non-fatal)");
            None
        }
    }
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
