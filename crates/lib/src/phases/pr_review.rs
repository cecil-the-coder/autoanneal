use crate::llm::{self, LlmInvocation};
use crate::models::{CriticResult, ExternalPr};
use crate::phases::critic::CriticOutput;
use crate::prompts::critic::CRITIC_PROMPT;
use crate::prompts::pr_review::PR_REVIEW_FIX_PROMPT;
use crate::prompts::system::{critic_system_prompt, pr_review_fix_system_prompt};
use crate::guardrails;
use crate::retry::gh_command;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// Maximum diff length (in characters) sent for review.
const MAX_DIFF_CHARS: usize = 50_000;

#[allow(dead_code)]
pub struct PrReviewOutput {
    pub pr_number: u64,
    pub score: u32,
    pub fixed: bool,
    pub commented: bool,
    pub cost_usd: f64,
}

pub async fn run(
    pr: &ExternalPr,
    repo_slug: &str,
    worktree_path: &Path,
    model: &str,
    fix_threshold: u32,
    context_window: u64,
    critic_models: Option<&[String]>,
    default_branch: &str,
) -> Result<PrReviewOutput> {
    let dot = Path::new(".");
    let clone_dir = worktree_path.to_path_buf();

    // 1. Get the diff using gh pr diff.
    let diff = match gh_command(
        dot,
        &[
            "pr",
            "diff",
            &pr.number.to_string(),
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => {
            if raw.chars().count() > MAX_DIFF_CHARS {
                // Find safe UTF-8 boundary at MAX_DIFF_CHARS characters
                let truncate_at = raw
                    .char_indices()
                    .nth(MAX_DIFF_CHARS)
                    .map(|(idx, _)| idx)
                    .unwrap_or(raw.len());
                let mut truncated = raw[..truncate_at].to_string();
                truncated.push_str("\n\n... (diff truncated) ...");
                truncated
            } else {
                raw
            }
        }
        Err(e) => {
            warn!(pr_number = pr.number, error = %e, "failed to get PR diff");
            anyhow::bail!("failed to get diff for PR #{}: {e}", pr.number);
        }
    };

    if diff.trim().is_empty() {
        // No diff, mark as reviewed and return.
        add_reviewed_label(repo_slug, pr.number).await;
        return Ok(PrReviewOutput {
            pr_number: pr.number,
            score: 10,
            fixed: false,
            commented: false,
            cost_usd: 0.0,
        });
    }

    // 4. Run critic review — panel if configured, single critic otherwise.
    let critic_output: CriticOutput = if let Some(models) = critic_models {
        // Panel mode: skip Gate 1 (human PR, worthwhileness is assumed)
        // Pass the gh pr diff so the panel reviews the correct PR changes,
        // not a git diff that may include unrelated commits from main.
        info!(pr_number = pr.number, models = models.len(), "PR review using critic panel");
        super::critic_panel::run_with_diff(
            &clone_dir,
            default_branch,
            models,
            context_window,
            true, // skip_gate1 — human PRs are assumed worthwhile
            0,    // no web searches for PR reviews
            Some(&diff),
        )
        .await
        .unwrap_or(CriticOutput {
            score: 5,
            verdict: "needs_work".to_string(),
            summary: "Critic panel failed.".to_string(),
            cost_usd: 0.0,
            made_fixes: false,
            score_unverified: false,
            initial_summary: None,
            initial_score: None,
        })
    } else {
        // Single critic mode
        let critic_prompt = CRITIC_PROMPT.replace("{diff}", &diff);
        let critic_invocation = LlmInvocation {
            prompt: critic_prompt,
            system_prompt: Some(critic_system_prompt()),
            model: model.to_string(),
            max_turns: 30,
            effort: "high",
            tools: "Read,Glob,Grep,Bash",
            json_schema: None,
            working_dir: clone_dir.clone(),
            context_window,
            provider_hint: None,
            max_tokens_per_turn: None,
            ci_context: None,
            exa_max_searches: 0,
        };

        let critic_response =
            llm::invoke::<CriticResult>(&critic_invocation, Duration::from_secs(300)).await?;

        let critic = if let Some(structured) = critic_response.structured {
            structured
        } else {
            let text_preview: String = critic_response.text.chars().take(500).collect();
            warn!(
                pr_number = pr.number,
                text_len = critic_response.text.len(),
                text_preview = %text_preview,
                "critic did not return parseable JSON, using fallback score"
            );
            CriticResult {
                score: 5,
                verdict: "needs_work".to_string(),
                summary: format!(
                    "Critic did not return structured output. Raw response: {}",
                    text_preview
                ),
                deductions: vec![],
            }
        };

        // Append deductions to summary so the fix agent knows exactly what to address.
        let summary = if critic.deductions.is_empty() {
            critic.summary
        } else {
            format!(
                "{}\n\nDeductions:\n{}",
                critic.summary,
                critic.deductions.iter().map(|d| format!("- {d}")).collect::<Vec<_>>().join("\n")
            )
        };

        CriticOutput {
            score: critic.score,
            verdict: critic.verdict,
            summary,
            cost_usd: critic_response.cost_usd,
            made_fixes: false,
            score_unverified: false,
            initial_summary: None,
            initial_score: None,
        }
    };

    let mut total_cost = critic_output.cost_usd;

    info!(
        pr_number = pr.number,
        score = critic_output.score,
        verdict = %critic_output.verdict,
        "PR review critic complete"
    );

    // 5. If score >= fix_threshold, the PR looks fine. Comment and label.
    if critic_output.score >= fix_threshold {
        let comment = format!(
            "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}",
            critic_output.score, critic_output.verdict, critic_output.summary
        );
        leave_comment(repo_slug, pr.number, &comment).await;
        add_reviewed_label(repo_slug, pr.number).await;
        if critic_output.score >= 10 {
            add_ready_to_merge_label(repo_slug, pr.number).await;
        }
        return Ok(PrReviewOutput {
            pr_number: pr.number,
            score: critic_output.score,
            fixed: false,
            commented: true,
            cost_usd: total_cost,
        });
    }

    // 6. Score < fix_threshold -- the PR needs work. Try to fix it.
    // Don't attempt fixes on rejected PRs -- they shouldn't exist at all.
    if critic_output.verdict == "reject" {
        let comment = format!(
            "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}",
            critic_output.score, critic_output.verdict, critic_output.summary
        );
        leave_comment(repo_slug, pr.number, &comment).await;
        add_reviewed_label(repo_slug, pr.number).await;
        return Ok(PrReviewOutput {
            pr_number: pr.number,
            score: critic_output.score,
            fixed: false,
            commented: true,
            cost_usd: total_cost,
        });
    }

    // 6a. Invoke LLM with fix prompt.
    let fix_prompt = PR_REVIEW_FIX_PROMPT
        .replace("{pr_number}", &pr.number.to_string())
        .replace("{branch}", &pr.branch)
        .replace("{score}", &critic_output.score.to_string())
        .replace("{summary}", &critic_output.summary)
        .replace("{diff}", &diff);

    let fix_invocation = LlmInvocation {
        prompt: fix_prompt,
        system_prompt: Some(pr_review_fix_system_prompt()),
        model: model.to_string(),
        max_turns: 100,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: clone_dir.clone(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: None,
        ci_context: None,
        exa_max_searches: 0,
    };

    let fix_response: llm::LlmResponse<serde_json::Value> =
        llm::invoke(&fix_invocation, Duration::from_secs(600)).await?;

    total_cost += fix_response.cost_usd;

    // 6b. Check for changes.
    let has_changes = check_has_changes(&clone_dir).await;

    if has_changes {
        // Validate diff against guardrails before committing.
        info!(pr_number = pr.number, "validating PR review fix diff against guardrails");
        if let Err(violation) = guardrails::validate_diff(&clone_dir, &[], 500, false).await {
            warn!(
                pr_number = pr.number,
                violation = %violation,
                "guardrail violation, discarding PR review fix changes"
            );
            // Discard fix agent changes and undo the snapshot commit.
            let _ = guardrails::discard_changes(&clone_dir).await;
            let _ = tokio::process::Command::new("git")
                .args(["reset", "--soft", "HEAD~1"])
                .current_dir(&clone_dir)
                .output()
                .await;
            // Leave a comment so the PR author knows fixes were attempted but rejected.
            let comment = format!(
                "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated fixes were generated but discarded due to safety guardrails ({}). Please review the suggestions above._",
                critic_output.score, critic_output.verdict, critic_output.summary, violation
            );
            leave_comment(repo_slug, pr.number, &comment).await;
            add_reviewed_label(repo_slug, pr.number).await;

            return Ok(PrReviewOutput {
                pr_number: pr.number,
                score: critic_output.score,
                fixed: false,
                commented: true,
                cost_usd: total_cost,
            });
        } else {
            // Stage and commit changes.
            let commit_ok = commit_changes(&clone_dir).await.is_ok();

            if commit_ok {
                // Try to push.
                let push_ok = push_changes(&clone_dir, &pr.branch).await.is_ok();

                if push_ok {
                    // Re-review the fixed diff to get an updated score.
                    let (final_score, final_verdict, final_summary) = {
                        info!(pr_number = pr.number, "re-reviewing after fixes");
                        match run_critic_review(
                            &clone_dir, repo_slug, pr, model, context_window,
                            critic_output.score, &critic_output.summary,
                        ).await {
                            Ok((output, cost)) => {
                                total_cost += cost;
                                info!(
                                    pr_number = pr.number,
                                    initial_score = critic_output.score,
                                    new_score = output.score,
                                    "re-review complete"
                                );
                                (output.score, output.verdict, output.summary)
                            }
                            Err(e) => {
                                warn!(pr_number = pr.number, error = %e, "re-review failed, using initial score");
                                (critic_output.score, critic_output.verdict.clone(), "Re-review could not be completed.".to_string())
                            }
                        }
                    };

                    // If the re-review scored lower, the fixes made things worse.
                    // Revert and comment with just the initial review.
                    if final_score < critic_output.score {
                        warn!(
                            pr_number = pr.number,
                            initial_score = critic_output.score,
                            final_score,
                            "re-review scored lower after fixes, reverting"
                        );
                        // Revert: reset to the pre-fix snapshot's parent (the original PR HEAD).
                        let _ = tokio::process::Command::new("git")
                            .args(["reset", "--hard", "HEAD~1"])
                            .current_dir(&clone_dir)
                            .output()
                            .await;
                        let _ = push_changes(&clone_dir, &pr.branch).await;

                        let comment = format!(
                            "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated fixes were attempted but reverted (re-review scored {}/10, lower than the original {}/10)._",
                            critic_output.score, critic_output.verdict, critic_output.summary,
                            final_score, critic_output.score,
                        );
                        leave_comment(repo_slug, pr.number, &comment).await;
                        add_reviewed_label(repo_slug, pr.number).await;

                        return Ok(PrReviewOutput {
                            pr_number: pr.number,
                            score: critic_output.score,
                            fixed: false,
                            commented: true,
                            cost_usd: total_cost,
                        });
                    }

                    let comment = format!(
                        "## Autoanneal Review & Fix\n\n**Score:** {}/10 → {}/10\n**Verdict:** {} → {}\n\n### Issues Found\n{}\n\n### After Fix\n{}\n\n_Automated fixes have been pushed to this branch._",
                        critic_output.score, final_score,
                        critic_output.verdict, final_verdict,
                        critic_output.summary, final_summary,
                    );
                    leave_comment(repo_slug, pr.number, &comment).await;
                    add_reviewed_label(repo_slug, pr.number).await;
                    if final_score >= 10 {
                        add_ready_to_merge_label(repo_slug, pr.number).await;
                    }

                    return Ok(PrReviewOutput {
                        pr_number: pr.number,
                        score: final_score,
                        fixed: true,
                        commented: true,
                        cost_usd: total_cost,
                    });
                } else {
                    // Push failed (no permission / protected branch). Leave review comment.
                    let comment = format!(
                        "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated fixes were prepared but could not be pushed (insufficient permissions or protected branch). Please review the suggestions above._",
                        critic_output.score, critic_output.verdict, critic_output.summary
                    );
                    leave_comment(repo_slug, pr.number, &comment).await;
                    add_reviewed_label(repo_slug, pr.number).await;

                    return Ok(PrReviewOutput {
                        pr_number: pr.number,
                        score: critic_output.score,
                        fixed: false,
                        commented: true,
                        cost_usd: total_cost,
                    });
                }
            }
        }
    }

    // 6c. No changes made. Leave review comment.
    let comment = format!(
        "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}",
        critic_output.score, critic_output.verdict, critic_output.summary
    );
    leave_comment(repo_slug, pr.number, &comment).await;
    add_reviewed_label(repo_slug, pr.number).await;

    Ok(PrReviewOutput {
        pr_number: pr.number,
        score: critic_output.score,
        fixed: false,
        commented: true,
        cost_usd: total_cost,
    })
}

/// Check if the working tree has uncommitted changes.
async fn check_has_changes(clone_dir: &Path) -> bool {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(clone_dir)
        .output()
        .await;

    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            !text.trim().is_empty()
        }
        Err(_) => false,
    }
}

/// Stage all changes and create a commit.
async fn commit_changes(clone_dir: &Path) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to run git add")?;
    if !output.status.success() {
        anyhow::bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Amend the pre-fix snapshot commit so the PR gets a single clean commit
    // instead of snapshot + fix.
    let output = tokio::process::Command::new("git")
        .args(["commit", "--amend", "-m", "autoanneal: fix issues found in PR review"])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to run git commit --amend")?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit --amend failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Push changes to the remote branch.
async fn push_changes(clone_dir: &Path, branch: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["push", "--force-with-lease", "origin", &format!("HEAD:refs/heads/{branch}")])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to run git push")?;
    if !output.status.success() {
        anyhow::bail!(
            "git push failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Leave a comment on a PR.
async fn leave_comment(repo_slug: &str, pr_number: u64, body: &str) {
    let dot = Path::new(".");
    if let Err(e) = gh_command(
        dot,
        &[
            "pr",
            "comment",
            &pr_number.to_string(),
            "--body",
            body,
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        warn!(pr_number, error = %e, "failed to leave PR comment (non-fatal)");
    }
}

/// Create the reviewed label (idempotent) and add it to the PR.
async fn add_reviewed_label(repo_slug: &str, pr_number: u64) {
    let dot = Path::new(".");

    // Create label (force = idempotent).
    let _ = gh_command(
        dot,
        &[
            "label",
            "create",
            "autoanneal:reviewed",
            "--color",
            "0E8A16",
            "--force",
            "-R",
            repo_slug,
        ],
    )
    .await;

    // Add label to PR.
    if let Err(e) = gh_command(
        dot,
        &[
            "pr",
            "edit",
            &pr_number.to_string(),
            "--add-label",
            "autoanneal:reviewed",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        warn!(pr_number, error = %e, "failed to add autoanneal:reviewed label (non-fatal)");
    }
}

async fn add_ready_to_merge_label(repo_slug: &str, pr_number: u64) {
    let dot = Path::new(".");
    if let Err(e) = gh_command(
        dot,
        &[
            "pr", "edit", &pr_number.to_string(),
            "--add-label", "autoanneal:ready-to-merge",
            "-R", repo_slug,
        ],
    )
    .await
    {
        warn!(pr_number, error = %e, "failed to add autoanneal:ready-to-merge label (non-fatal)");
    }
}

/// Run a re-review anchored to the initial review's score and deductions.
/// The re-review checks whether the fix agent addressed the deductions,
/// rather than scoring the PR from scratch.
/// Returns (CriticOutput, cost) on success.
async fn run_critic_review(
    clone_dir: &Path,
    repo_slug: &str,
    pr: &ExternalPr,
    model: &str,
    context_window: u64,
    initial_score: u32,
    initial_deductions: &str,
) -> Result<(CriticOutput, f64)> {
    // Get the updated diff.
    let diff_output = tokio::process::Command::new("gh")
        .args([
            "pr", "diff", &pr.number.to_string(),
            "-R", repo_slug,
        ])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to get PR diff for re-review")?;

    let diff = String::from_utf8_lossy(&diff_output.stdout);
    let diff = llm::truncate_to_char_boundary(&diff, MAX_DIFF_CHARS);

    let critic_prompt = format!(
        r#"You are re-reviewing a PR after automated fixes were applied.

The initial review scored this PR {initial_score}/10 with these deductions:

{initial_deductions}

A fix agent attempted to address these deductions. Review the updated diff below.

For each original deduction:
- If it was fixed, remove that deduction (add points back toward 10)
- If it was NOT fixed, keep it

If the fixes introduced NEW issues not in the original review, add new deductions for those.

IMPORTANT:
- Start from {initial_score}/10 and adjust: +1 per resolved deduction, -1 per new issue
- Each distinct issue should appear EXACTLY ONCE in your deductions list. Do NOT list the same issue with different wording. Consolidate similar concerns into a single deduction.
- Each deduction must specify how many points it costs (e.g. "-1: description")
- The total point deductions must equal (10 - score)

## Updated Diff

```
{diff}
```

Output a JSON code block:

```json
{{
  "score": 9,
  "verdict": "approve|needs_work|reject",
  "summary": "Brief summary of what changed since the initial review",
  "deductions": ["-1: One unique issue per line"]
}}
```"#,
        initial_score = initial_score,
        initial_deductions = initial_deductions,
        diff = diff,
    );
    let invocation = LlmInvocation {
        prompt: critic_prompt,
        system_prompt: Some("You are a code reviewer re-evaluating a PR after automated fixes. You MUST respond with ONLY a JSON code block. No reasoning, no analysis, no explanation — just the JSON. Any text outside the JSON block will cause a parse failure.".to_string()),
        model: model.to_string(),
        max_turns: 1,
        effort: "high",
        tools: "",
        json_schema: None,
        working_dir: clone_dir.to_path_buf(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: Some(16384),
        ci_context: None,
        exa_max_searches: 0,
    };

    let response = llm::invoke::<CriticResult>(&invocation, Duration::from_secs(120)).await?;
    let cost = response.cost_usd;

    let critic = if let Some(structured) = response.structured {
        structured
    } else {
        let text_preview: String = response.text.chars().take(500).collect();
        warn!(
            pr_number = pr.number,
            text_len = response.text.len(),
            text_preview = %text_preview,
            "re-review did not return parseable JSON, using fallback score"
        );
        CriticResult {
            score: 5,
            verdict: "needs_work".to_string(),
            summary: format!(
                "Re-review did not return structured output. Raw response: {}",
                text_preview
            ),
            deductions: vec![],
        }
    };

    Ok((CriticOutput {
        score: critic.score,
        verdict: critic.verdict,
        summary: critic.summary,
        cost_usd: cost,
        made_fixes: false,
        score_unverified: false,
        initial_summary: None,
        initial_score: None,
    }, cost))
}
