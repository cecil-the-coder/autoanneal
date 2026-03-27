use crate::claude::{self, ClaudeInvocation, generate_session_id};
use crate::models::{CriticResult, ExternalPr};
use crate::prompts::critic::CRITIC_PROMPT;
use crate::prompts::pr_review::PR_REVIEW_FIX_PROMPT;
use crate::prompts::system::{critic_system_prompt, pr_review_fix_system_prompt};
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
    budget: f64,
    fix_threshold: u32,
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
            // Quick byte-length check (O(1)) to avoid expensive char iteration for small diffs.
            // Byte length >= char count, so if bytes <= MAX_DIFF_CHARS, we're safe.
            if raw.len() > MAX_DIFF_CHARS {
                // Find safe UTF-8 boundary at MAX_DIFF_CHARS characters.
                // This iterates only up to MAX_DIFF_CHARS, not the whole string.
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

    // 4. Run critic on the diff.
    let critic_prompt = CRITIC_PROMPT.replace("{diff}", &diff);
    let critic_budget = budget * 0.30;

    let critic_invocation = ClaudeInvocation {
        prompt: critic_prompt,
        system_prompt: Some(critic_system_prompt()),
        model: model.to_string(),
        max_budget_usd: critic_budget,
        max_turns: 30,
        effort: "high",
        tools: "Read,Glob,Grep,Bash",
        json_schema: None,
        working_dir: clone_dir.clone(),
        session_id: None,
        resume_session_id: None,
    };

    let critic_response =
        claude::invoke::<CriticResult>(&critic_invocation, Duration::from_secs(300)).await?;

    let mut total_cost = critic_response.cost_usd;

    let critic = critic_response.structured.unwrap_or(CriticResult {
        score: 5,
        verdict: "needs_work".to_string(),
        summary: "Critic did not return structured output.".to_string(),
    });

    info!(
        pr_number = pr.number,
        score = critic.score,
        verdict = %critic.verdict,
        "PR review critic complete"
    );

    // 5. If score >= fix_threshold, the PR looks fine. Just label and move on.
    if critic.score >= fix_threshold {
        add_reviewed_label(repo_slug, pr.number).await;
        return Ok(PrReviewOutput {
            pr_number: pr.number,
            score: critic.score,
            fixed: false,
            commented: false,
            cost_usd: total_cost,
        });
    }

    // 6. Score < fix_threshold -- the PR needs work. Try to fix it.
    let remaining_budget = (budget - total_cost).max(0.0);
    if remaining_budget < 0.10 {
        // Not enough budget to attempt fixes; just comment.
        let comment = format!(
            "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated review by autoanneal. Not enough budget remaining to attempt fixes._",
            critic.score, critic.verdict, critic.summary
        );
        leave_comment(repo_slug, pr.number, &comment).await;
        add_reviewed_label(repo_slug, pr.number).await;
        return Ok(PrReviewOutput {
            pr_number: pr.number,
            score: critic.score,
            fixed: false,
            commented: true,
            cost_usd: total_cost,
        });
    }

    // 6a. Invoke Claude with fix prompt.
    let fix_prompt = PR_REVIEW_FIX_PROMPT
        .replace("{pr_number}", &pr.number.to_string())
        .replace("{branch}", &pr.branch)
        .replace("{score}", &critic.score.to_string())
        .replace("{summary}", &critic.summary)
        .replace("{diff}", &diff);

    let session_id = generate_session_id();

    let fix_invocation = ClaudeInvocation {
        prompt: fix_prompt,
        system_prompt: Some(pr_review_fix_system_prompt()),
        model: model.to_string(),
        max_budget_usd: remaining_budget,
        max_turns: 100,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: clone_dir.clone(),
        session_id: Some(session_id),
        resume_session_id: None,
    };

    let fix_response: claude::ClaudeResponse<serde_json::Value> =
        claude::invoke(&fix_invocation, Duration::from_secs(600)).await?;

    total_cost += fix_response.cost_usd;

    // 6b. Check if Claude made changes.
    let has_changes = check_has_changes(&clone_dir).await;

    if has_changes {
        // Stage and commit changes.
        let commit_ok = commit_changes(&clone_dir).await.is_ok();

        if commit_ok {
            // Try to push.
            let push_ok = push_changes(&clone_dir, &pr.branch).await.is_ok();

            if push_ok {
                // Leave a comment summarizing what was fixed.
                let comment = format!(
                    "## Autoanneal Review & Fix\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated fixes have been pushed to this branch._",
                    critic.score, critic.verdict, critic.summary
                );
                leave_comment(repo_slug, pr.number, &comment).await;
                add_reviewed_label(repo_slug, pr.number).await;

                return Ok(PrReviewOutput {
                    pr_number: pr.number,
                    score: critic.score,
                    fixed: true,
                    commented: true,
                    cost_usd: total_cost,
                });
            } else {
                // Push failed (no permission / protected branch). Leave review comment.
                let comment = format!(
                    "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}\n\n_Automated fixes were prepared but could not be pushed (insufficient permissions or protected branch). Please review the suggestions above._",
                    critic.score, critic.verdict, critic.summary
                );
                leave_comment(repo_slug, pr.number, &comment).await;
                add_reviewed_label(repo_slug, pr.number).await;

                return Ok(PrReviewOutput {
                    pr_number: pr.number,
                    score: critic.score,
                    fixed: false,
                    commented: true,
                    cost_usd: total_cost,
                });
            }
        }
    }

    // 6c. No changes made by Claude. Leave review comment.
    let comment = format!(
        "## Autoanneal Review\n\n**Score:** {}/10\n**Verdict:** {}\n\n{}",
        critic.score, critic.verdict, critic.summary
    );
    leave_comment(repo_slug, pr.number, &comment).await;
    add_reviewed_label(repo_slug, pr.number).await;

    Ok(PrReviewOutput {
        pr_number: pr.number,
        score: critic.score,
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

    let output = tokio::process::Command::new("git")
        .args(["commit", "-m", "autoanneal: fix issues found in PR review"])
        .current_dir(clone_dir)
        .output()
        .await
        .context("failed to run git commit")?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Push changes to the remote branch.
async fn push_changes(clone_dir: &Path, branch: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["push", "origin", branch])
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
