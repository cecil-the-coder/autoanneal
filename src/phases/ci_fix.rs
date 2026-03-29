use crate::agent::tools::CiContext;
use crate::llm::{self, truncate_to_char_boundary, LlmInvocation};
use crate::models::InFlightPr;
use crate::prompts;
use crate::retry::gh_command;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

#[allow(dead_code)]
pub struct CiFixOutput {
    pub pr_number: u64,
    pub fixed: bool,
    pub cost_usd: f64,
}

/// Drop guard that removes the autoanneal:fixing label when dropped.
struct FixingLabelGuard {
    pr_number: u64,
    repo_slug: String,
}

impl Drop for FixingLabelGuard {
    fn drop(&mut self) {
        info!(
            pr_number = self.pr_number,
            "removing autoanneal:fixing label"
        );
        let _ = std::process::Command::new("gh")
            .args([
                "pr",
                "edit",
                &self.pr_number.to_string(),
                "--remove-label",
                "autoanneal:fixing",
                "-R",
                &self.repo_slug,
            ])
            .output();
    }
}

pub async fn run(
    pr: &InFlightPr,
    repo_slug: &str,
    worktree_path: &Path,
    model: &str,
    budget: f64,
    default_branch: &str,
    context_window: u64,
) -> Result<CiFixOutput> {
    let dot = Path::new(".");
    let clone_dir = worktree_path.to_path_buf();

    // 1. Create label (force = idempotent) and add it to the PR.
    let _ = gh_command(
        dot,
        &[
            "label",
            "create",
            "autoanneal:fixing",
            "--color",
            "D93F0B",
            "--force",
            "-R",
            repo_slug,
        ],
    )
    .await;

    gh_command(
        dot,
        &[
            "pr",
            "edit",
            &pr.number.to_string(),
            "--add-label",
            "autoanneal:fixing",
            "-R",
            repo_slug,
        ],
    )
    .await
    .context("failed to add autoanneal:fixing label")?;

    // Set up drop guard to always remove the label.
    let _label_guard = FixingLabelGuard {
        pr_number: pr.number,
        repo_slug: repo_slug.to_string(),
    };

    // 2. Handle merge conflicts: fetch and merge default branch first.
    if pr.has_merge_conflicts {
        info!(pr_number = pr.number, default_branch = default_branch, "PR has merge conflicts, attempting rebase on default branch");
        let _ = tokio::process::Command::new("git")
            .args(["fetch", "origin", default_branch])
            .current_dir(&clone_dir)
            .output()
            .await;

        let merge_output = tokio::process::Command::new("git")
            .args(["merge", &format!("origin/{default_branch}"), "--no-edit"])
            .current_dir(&clone_dir)
            .output()
            .await;

        match merge_output {
            Ok(out) if out.status.success() => {
                info!(pr_number = pr.number, default_branch = default_branch, "merged default branch successfully, no conflicts remain");
                // Push the merge commit directly — no Claude needed
                let push_result = commit_and_push(&clone_dir, &pr.branch).await;
                return Ok(CiFixOutput {
                    pr_number: pr.number,
                    fixed: push_result.is_ok(),
                    cost_usd: 0.0,
                });
            }
            _ => {
                // Merge has conflicts — let Claude resolve them
                info!(pr_number = pr.number, "merge conflicts detected, invoking Claude to resolve");
            }
        }
    }

    // 5. Fetch CI logs (for CI failures) or conflict markers (for merge conflicts).
    let (context, tools, ci_context): (String, &'static str, Option<CiContext>) = if pr.has_merge_conflicts {
        // Get conflict markers from working tree
        let output = tokio::process::Command::new("git")
            .args(["diff"])
            .current_dir(&clone_dir)
            .output()
            .await;
        let diff_text = match output {
            Ok(out) => {
                let diff = String::from_utf8_lossy(&out.stdout).to_string();
                truncate_to_char_boundary(&diff, 50_000)
            }
            Err(_) => "(could not get conflict diff)".to_string(),
        };
        (diff_text, "Read,Glob,Grep,Edit,Write,Git", None)
    } else {
        let (ci_logs, run_id) = fetch_ci_logs(repo_slug, &pr.branch).await;
        let truncated = truncate_to_char_boundary(&ci_logs, 50_000);
        let ctx = CiContext {
            repo_slug: repo_slug.to_string(),
            run_id,
        };
        (truncated, "Read,Glob,Grep,Edit,Write,GhWorkflowLogs,Git", Some(ctx))
    };

    // 6. Invoke Claude.
    let prompt = if pr.has_merge_conflicts {
        format!(
            "Pull request #{} (branch: {}) has merge conflicts with {}.\n\n\
             ## Conflict Diff\n\n```\n{}\n```\n\n\
             ## Instructions\n\n\
             Resolve the merge conflicts. For each conflicted file, choose the correct resolution \
             (keep ours, keep theirs, or combine). After resolving, ensure the code compiles and tests pass.",
            pr.number, pr.branch, default_branch, context
        )
    } else {
        prompts::ci_fix::CI_FIX_PROMPT
            .replace("{pr_number}", &pr.number.to_string())
            .replace("{branch_name}", &pr.branch)
            .replace("{ci_logs}", &context)
            .replace("{pr_title}", &pr.title)
    };

    let system_prompt = prompts::system::ci_fix_system_prompt();

    let invocation = LlmInvocation {
        prompt,
        system_prompt: Some(system_prompt),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 30,
        effort: "high",
        tools,
        json_schema: None,
        working_dir: clone_dir.clone(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: None,
        ci_context,
        exa_max_searches: 0,
    };

    let response: llm::LlmResponse<serde_json::Value> =
        llm::invoke(&invocation, Duration::from_secs(900)).await?;

    let cost_usd = response.cost_usd;

    // 6. Commit and push.
    let commit_result = commit_and_push(&clone_dir, &pr.branch).await;
    let fixed = commit_result.is_ok();

    if let Err(e) = &commit_result {
        warn!(error = %e, "ci-fix commit/push failed");
    }

    Ok(CiFixOutput {
        pr_number: pr.number,
        fixed,
        cost_usd,
    })
}


/// Fetch CI logs and run_id for the latest run on `branch`.
/// Returns (combined_logs, run_id). run_id is 0 if not found.
async fn fetch_ci_logs(repo_slug: &str, branch: &str) -> (String, u64) {
    let dot = Path::new(".");

    // Get latest run ID
    let run_id = match gh_command(
        dot,
        &[
            "run",
            "list",
            "--branch",
            branch,
            "--limit",
            "1",
            "--json",
            "databaseId",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => {
            let runs: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return (String::from("(could not parse CI run list)"), 0),
            };
            match runs.first().and_then(|r| r["databaseId"].as_u64()) {
                Some(id) => id,
                None => return (String::from("(no CI runs found)"), 0),
            }
        }
        Err(e) => return (format!("(failed to list CI runs: {e})"), 0),
    };

    // Fetch structured job info
    let job_summary = match gh_command(
        dot,
        &[
            "run",
            "view",
            &run_id.to_string(),
            "--json",
            "jobs",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(json_str) => format_job_summary(&json_str),
        Err(e) => format!("(failed to fetch job info: {e})"),
    };

    // Get failed logs
    let failed_logs = match gh_command(
        dot,
        &[
            "run",
            "view",
            &run_id.to_string(),
            "--log-failed",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(logs) => logs,
        Err(e) => format!("(failed to fetch CI logs: {e})"),
    };

    let combined = format!(
        "## Job Summary\n\n{job_summary}\n\n## Failed Logs\n\n{failed_logs}"
    );
    (combined, run_id)
}

/// Parse the `gh run view --json jobs` output and format a summary of which
/// jobs/steps failed, including their job IDs (for use with `gh_workflow_logs`).
fn format_job_summary(json_str: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return "(could not parse jobs JSON)".to_string(),
    };

    let jobs = match parsed.get("jobs").and_then(|j| j.as_array()) {
        Some(arr) => arr,
        None => return "(no jobs found in response)".to_string(),
    };

    let mut lines = Vec::new();
    for job in jobs {
        let name = job.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let conclusion = job.get("conclusion").and_then(|v| v.as_str()).unwrap_or("?");
        let job_id = job.get("databaseId").and_then(|v| v.as_u64()).unwrap_or(0);

        let marker = if conclusion == "failure" { "FAILED" } else { conclusion };
        lines.push(format!("- [{marker}] {name} (job_id: {job_id})"));

        // List failed steps within the job
        if conclusion == "failure" {
            if let Some(steps) = job.get("steps").and_then(|s| s.as_array()) {
                for step in steps {
                    let step_name = step.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let step_conclusion = step.get("conclusion").and_then(|v| v.as_str()).unwrap_or("?");
                    if step_conclusion == "failure" {
                        lines.push(format!("  - FAILED step: {step_name}"));
                    }
                }
            }
        }
    }

    if lines.is_empty() {
        "(no jobs found)".to_string()
    } else {
        lines.join("\n")
    }
}

async fn commit_and_push(clone_dir: &PathBuf, branch: &str) -> Result<()> {
    // git add -A
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

    // Check if there are changes to commit
    let status = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(clone_dir)
        .output()
        .await?;
    let status_text = String::from_utf8_lossy(&status.stdout);
    if status_text.trim().is_empty() {
        anyhow::bail!("no changes to commit after CI fix attempt");
    }

    // git commit
    let output = tokio::process::Command::new("git")
        .args(["commit", "-m", "autoanneal: fix CI failures"])
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

    // git push
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
