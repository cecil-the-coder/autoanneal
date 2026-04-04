use crate::llm::{self, LlmInvocation};
use crate::models::{CriticResult, ExternalPr};
use crate::phases::critic::CriticOutput;
use crate::prompts::critic::CRITIC_PROMPT;
use crate::prompts::pr_review::PR_REVIEW_FIX_SINGLE_DEDUCTION_PROMPT;
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

/// Maximum fix->re-review cycles before giving up.
const MAX_FIX_PASSES: u32 = 3;

/// Maximum build verification attempts per deduction fix.
const MAX_BUILD_ATTEMPTS: u32 = 2;

/// Build check timeout.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn run(
    pr: &ExternalPr,
    repo_slug: &str,
    worktree_path: &Path,
    model: &str,
    fix_threshold: u32,
    context_window: u64,
    critic_models: Option<&[String]>,
    default_branch: &str,
    exa_max_searches: u32,
    build_command: Option<&str>,
    max_deductions_per_pass: usize,
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

    // 4. Run critic review -- panel if configured, single critic otherwise.
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
            true, // skip_gate1 -- human PRs are assumed worthwhile
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

    // 6. Score < fix_threshold -- try multi-pass fix cycles.
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

    let initial_score = critic_output.score;
    let initial_summary = critic_output.summary.clone();
    let mut current_score = critic_output.score;
    let mut current_summary = critic_output.summary.clone();
    let mut current_verdict = critic_output.verdict.clone();
    let mut any_fixes_applied = false;
    let mut pass_summaries: Vec<String> = Vec::new();

    for pass in 0..MAX_FIX_PASSES {
        info!(
            pr_number = pr.number,
            pass = pass + 1,
            score = current_score,
            "starting fix pass"
        );

        // Parse deductions from the current summary.
        let deductions = parse_deductions(&current_summary);

        if deductions.is_empty() {
            info!(pr_number = pr.number, pass = pass + 1, "no deductions found, stopping");
            break;
        }

        let mut deductions_fixed = 0u32;
        let max_deductions = max_deductions_per_pass.min(deductions.len());

        // Process each deduction individually (per-deduction fix loop).
        for (ded_idx, deduction) in deductions.iter().take(max_deductions).enumerate() {
            info!(
                pr_number = pr.number,
                pass = pass + 1,
                deduction_index = ded_idx + 1,
                total_deductions = deductions.len(),
                deduction = %deduction,
                "fixing deduction"
            );

            // Snapshot before fix agent runs so guardrails only measure its changes.
            let _ = tokio::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(&clone_dir)
                .output()
                .await;
            let _ = tokio::process::Command::new("git")
                .args(["commit", "--allow-empty", "-m", "autoanneal: pre-fix snapshot"])
                .current_dir(&clone_dir)
                .output()
                .await;

            // Get fresh diff for context.
            let current_diff = get_pr_diff(repo_slug, pr.number, &clone_dir).await
                .unwrap_or_else(|_| diff.clone());

            let fix_prompt = PR_REVIEW_FIX_SINGLE_DEDUCTION_PROMPT
                .replace("{pr_number}", &pr.number.to_string())
                .replace("{branch}", &pr.branch)
                .replace("{deduction}", deduction)
                .replace("{diff}", &current_diff);

            let fix_invocation = LlmInvocation {
                prompt: fix_prompt,
                system_prompt: Some(pr_review_fix_system_prompt()),
                model: model.to_string(),
                max_turns: 30,
                effort: "high",
                tools: "Read,Glob,Grep,Bash,Edit,Write,WebSearch,CheckVulnerability,CheckPackage,SearchIssues",
                json_schema: None,
                working_dir: clone_dir.clone(),
                context_window,
                provider_hint: None,
                max_tokens_per_turn: None,
                ci_context: None,
                exa_max_searches,
            };

            let fix_response: llm::LlmResponse<serde_json::Value> =
                llm::invoke(&fix_invocation, Duration::from_secs(600)).await?;

            total_cost += fix_response.cost_usd;

            let has_changes = check_has_changes(&clone_dir).await;
            if !has_changes {
                info!(pr_number = pr.number, deduction = ded_idx + 1, "deduction fix made no changes, skipping");
                // Undo the snapshot.
                let _ = tokio::process::Command::new("git")
                    .args(["reset", "--soft", "HEAD~1"])
                    .current_dir(&clone_dir)
                    .output()
                    .await;
                continue;
            }

            // Validate guardrails (build verification runs once after all deductions).
            if let Err(violation) = guardrails::validate_diff(&clone_dir, &[], 500, false).await {
                warn!(
                    pr_number = pr.number,
                    deduction = ded_idx + 1,
                    violation = %violation,
                    "guardrail violation, discarding deduction changes"
                );
                let _ = guardrails::discard_changes(&clone_dir).await;
                let _ = tokio::process::Command::new("git")
                    .args(["reset", "--soft", "HEAD~1"])
                    .current_dir(&clone_dir)
                    .output()
                    .await;
                continue;
            }

            // Commit with a per-deduction message.
            let commit_msg = format!(
                "autoanneal: fix deduction {}: {}",
                ded_idx + 1,
                truncate_str(deduction, 72)
            );
            if commit_deduction(&clone_dir, &commit_msg).await.is_err() {
                continue;
            }

            deductions_fixed += 1;
        }

        if deductions_fixed == 0 {
            info!(
                pr_number = pr.number,
                pass = pass + 1,
                "no deductions were fixed this pass, stopping"
            );
            break;
        }

        // Build verification after all deductions (one build, not per-deduction).
        if let Some(cmd) = build_command {
            let mut build_passed = false;
            for build_attempt in 0..MAX_BUILD_ATTEMPTS {
                match run_build_check(&clone_dir, cmd).await {
                    Ok(()) => {
                        build_passed = true;
                        break;
                    }
                    Err(build_err) => {
                        if build_attempt + 1 >= MAX_BUILD_ATTEMPTS {
                            warn!(
                                pr_number = pr.number,
                                pass = pass + 1,
                                error = %build_err,
                                "build failed after retries, reverting all deduction commits"
                            );
                            break;
                        }
                        info!(
                            pr_number = pr.number,
                            pass = pass + 1,
                            attempt = build_attempt + 1,
                            "build failed, invoking fix agent with build error"
                        );
                        let build_fix_prompt = format!(
                            "The build command `{cmd}` failed after automated fixes. Fix the build error:\n\n```\n{build_err}\n```\n\nMake minimal changes to resolve the build failure.",
                        );
                        let build_fix_invocation = LlmInvocation {
                            prompt: build_fix_prompt,
                            system_prompt: Some(pr_review_fix_system_prompt()),
                            model: model.to_string(),
                            max_turns: 15,
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
                        match llm::invoke::<serde_json::Value>(&build_fix_invocation, Duration::from_secs(300)).await {
                            Ok(resp) => {
                                total_cost += resp.cost_usd;
                                // Stage and amend the build fix into the last deduction commit.
                                let _ = tokio::process::Command::new("git")
                                    .args(["add", "-A"])
                                    .current_dir(&clone_dir)
                                    .output()
                                    .await;
                                let _ = tokio::process::Command::new("git")
                                    .args(["commit", "--amend", "--no-edit"])
                                    .current_dir(&clone_dir)
                                    .output()
                                    .await;
                            }
                            Err(e) => {
                                warn!(error = %e, "build fix invocation failed");
                                break;
                            }
                        }
                    }
                }
            }
            if !build_passed {
                // Revert all deduction commits from this pass.
                let reset_target = format!("HEAD~{deductions_fixed}");
                let _ = tokio::process::Command::new("git")
                    .args(["reset", "--hard", &reset_target])
                    .current_dir(&clone_dir)
                    .output()
                    .await;
                pass_summaries.push(format!("Pass {}: reverted (build failed)", pass + 1));
                break;
            }
        }

        // Push all deduction commits at once.
        if push_changes(&clone_dir, &pr.branch).await.is_err() {
            break;
        }

        any_fixes_applied = true;

        // Re-review anchored to current score.
        info!(pr_number = pr.number, pass = pass + 1, "re-reviewing after fixes");
        match run_critic_review(
            &clone_dir, repo_slug, pr, model, context_window,
            current_score, &current_summary,
        ).await {
            Ok((output, cost)) => {
                total_cost += cost;
                info!(
                    pr_number = pr.number,
                    pass = pass + 1,
                    prev_score = current_score,
                    new_score = output.score,
                    fixed = deductions_fixed,
                    total_deductions = deductions.len(),
                    "re-review complete"
                );

                if output.score < current_score {
                    // Score regressed -- revert all deduction commits from this pass.
                    warn!(
                        pr_number = pr.number,
                        pass = pass + 1,
                        prev_score = current_score,
                        new_score = output.score,
                        "re-review scored lower, reverting pass"
                    );
                    let reset_target = format!("HEAD~{deductions_fixed}");
                    let _ = tokio::process::Command::new("git")
                        .args(["reset", "--hard", &reset_target])
                        .current_dir(&clone_dir)
                        .output()
                        .await;
                    let _ = push_changes(&clone_dir, &pr.branch).await;
                    pass_summaries.push(format!(
                        "Pass {}: reverted (score dropped to {}, fixed {}/{} deductions)",
                        pass + 1, output.score, deductions_fixed, deductions.len()
                    ));
                    break;
                }

                pass_summaries.push(format!(
                    "Pass {}: {} -> {} (fixed {}/{} deductions)",
                    pass + 1, current_score, output.score, deductions_fixed, deductions.len()
                ));
                current_score = output.score;
                current_verdict = output.verdict;
                current_summary = output.summary;

                // If we reached 10 or the score didn't improve, stop.
                if current_score >= 10 {
                    info!(pr_number = pr.number, "reached perfect score, stopping");
                    break;
                }
            }
            Err(e) => {
                warn!(pr_number = pr.number, pass = pass + 1, error = %e, "re-review failed");
                pass_summaries.push(format!("Pass {}: re-review failed", pass + 1));
                break;
            }
        }
    }

    // Build the final comment.
    if any_fixes_applied {
        let passes_text = pass_summaries.join("\n");
        let comment = format!(
            "## Autoanneal Review & Fix\n\n**Score:** {}/10 -> {}/10\n**Verdict:** {} -> {}\n\n### Issues Found\n{}\n\n### Fix Passes\n{}\n\n### After Fix\n{}\n\n_Automated fixes have been pushed to this branch._",
            initial_score, current_score,
            critic_output.verdict, current_verdict,
            initial_summary, passes_text, current_summary,
        );
        leave_comment(repo_slug, pr.number, &comment).await;
        add_reviewed_label(repo_slug, pr.number).await;
        if current_score >= 10 {
            add_ready_to_merge_label(repo_slug, pr.number).await;
        }

        Ok(PrReviewOutput {
            pr_number: pr.number,
            score: current_score,
            fixed: true,
            commented: true,
            cost_usd: total_cost,
        })
    } else {
        // No fixes applied (no changes, guardrail violations, or all passes reverted).
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

/// Stage all changes and create a commit with a per-deduction message.
/// Amends the pre-fix snapshot commit so the history stays clean.
async fn commit_deduction(clone_dir: &Path, message: &str) -> Result<()> {
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

    // Amend the pre-fix snapshot commit so the PR gets one clean commit per deduction.
    let output = tokio::process::Command::new("git")
        .args(["commit", "--amend", "-m", message])
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

/// Parse deductions from a critic summary string.
///
/// Deductions appear in the summary as:
/// ```text
/// \nDeductions:\n- item 1\n- item 2
/// ```
/// or from the panel as numbered/bulleted lines after "Deductions:".
fn parse_deductions(summary: &str) -> Vec<String> {
    let mut deductions = Vec::new();
    let mut in_deductions = false;

    for line in summary.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("deductions:")
            || trimmed.eq_ignore_ascii_case("## deductions")
            || trimmed.eq_ignore_ascii_case("## score deductions")
            || trimmed.eq_ignore_ascii_case("## issues")
            || trimmed.eq_ignore_ascii_case("issues:")
        {
            in_deductions = true;
            continue;
        }
        if in_deductions {
            // Stop at next section header.
            if trimmed.starts_with("## ") {
                break;
            }
            // Parse bulleted items: "- item" or "* item" or "1. item" or "- -1: item"
            let item = trimmed
                .trim_start_matches('-')
                .trim_start_matches('*')
                .trim_start();
            // Handle numbered items like "1. item"
            let item = if let Some(rest) = item.strip_prefix(|c: char| c.is_ascii_digit()) {
                rest.trim_start_matches(|c: char| c.is_ascii_digit())
                    .trim_start_matches('.')
                    .trim_start()
            } else {
                item
            };
            if !item.is_empty() {
                deductions.push(item.to_string());
            }
        }
    }
    deductions
}

/// Format deductions as a numbered task list for the fix prompt.
#[allow(dead_code)]
fn format_tasks(deductions: &[String]) -> String {
    if deductions.is_empty() {
        return String::new();
    }
    let mut tasks = String::from("## Tasks to Complete\n\n");
    for (i, d) in deductions.iter().enumerate() {
        tasks.push_str(&format!("{}. {}\n", i + 1, d));
    }
    tasks
}

/// Run a build command and return Ok(()) on success or Err with the build output on failure.
async fn run_build_check(clone_dir: &Path, build_cmd: &str) -> Result<(), String> {
    let result = tokio::time::timeout(
        BUILD_TIMEOUT,
        tokio::process::Command::new("bash")
            .args(["-c", build_cmd])
            .current_dir(clone_dir)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = if stderr.trim().is_empty() {
                stdout.to_string()
            } else {
                format!("{}\n{}", stdout, stderr)
            };
            // Truncate to avoid huge error messages.
            let truncated: String = combined.chars().take(3000).collect();
            Err(truncated)
        }
        Ok(Err(e)) => Err(format!("failed to spawn build command: {e}")),
        Err(_) => Err("build command timed out after 2 minutes".to_string()),
    }
}

/// Truncate a string to at most `max_len` characters.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
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

/// Get the current PR diff.
async fn get_pr_diff(repo_slug: &str, pr_number: u64, clone_dir: &Path) -> Result<String> {
    let output = tokio::process::Command::new("gh")
        .args(["pr", "diff", &pr_number.to_string(), "-R", repo_slug])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to get PR diff")?;
    let diff = String::from_utf8_lossy(&output.stdout);
    Ok(llm::truncate_to_char_boundary(&diff, MAX_DIFF_CHARS))
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
        system_prompt: Some("You are a code reviewer re-evaluating a PR after automated fixes. You MUST respond with ONLY a JSON code block. No reasoning, no analysis, no explanation -- just the JSON. Any text outside the JSON block will cause a parse failure.".to_string()),
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
