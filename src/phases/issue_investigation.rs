use crate::llm::{self, LlmInvocation};
use crate::models::{GithubIssue, RepoInfo, StackInfo};
use crate::prompts;
use crate::guardrails;
use crate::retry::gh_command;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

#[allow(dead_code)]
pub struct IssueOutput {
    pub issue_number: u64,
    pub fixed: bool,
    pub pr_url: Option<String>,
    pub cost_usd: f64,
}

pub async fn run(
    issue: &GithubIssue,
    worktree_path: &Path,
    repo_slug: &str,
    _repo_info: &RepoInfo,
    arch_summary: &str,
    stack_info: &StackInfo,
    model: &str,
    budget: f64,
    context_window: u64,
) -> Result<IssueOutput> {
    let dot = Path::new(".");

    // 1. Add autoanneal:investigating label.
    let _ = gh_command(
        dot,
        &[
            "label",
            "create",
            "autoanneal:investigating",
            "--color",
            "FBCA04",
            "--force",
            "-R",
            repo_slug,
        ],
    )
    .await;

    let _ = gh_command(
        dot,
        &[
            "issue",
            "edit",
            &issue.number.to_string(),
            "--add-label",
            "autoanneal:investigating",
            "-R",
            repo_slug,
        ],
    )
    .await;

    // 2. Build the prompt.
    let build_cmds = stack_info.build_commands.join(", ");
    let test_cmds = stack_info.test_commands.join(", ");

    let prompt = prompts::issue_investigation::ISSUE_INVESTIGATION_PROMPT
        .replace("{issue_number}", &issue.number.to_string())
        .replace("{issue_title}", &issue.title)
        .replace("{issue_body}", &issue.body)
        .replace("{arch_summary}", arch_summary)
        .replace("{build_commands}", &build_cmds)
        .replace("{test_commands}", &test_cmds);

    let system_prompt = prompts::system::issue_investigation_system_prompt();

    // 3. Invoke Claude.
    let invocation = LlmInvocation {
        prompt,
        system_prompt: Some(system_prompt),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 100,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: worktree_path.to_path_buf(),
        context_window,
        provider_hint: None,
        max_tokens_per_turn: None,
    };

    let response: llm::LlmResponse<serde_json::Value> =
        llm::invoke(&invocation, Duration::from_secs(900)).await?;

    let cost_usd = response.cost_usd;

    // 4. Parse the result to check if fixed.
    let mut fixed = response
        .structured
        .as_ref()
        .and_then(|v| v.get("fixed"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let summary = response
        .structured
        .as_ref()
        .and_then(|v| v.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("Investigation completed.")
        .to_string();

    let mut pr_url = None;

    if fixed {
        // Validate diff against guardrails before committing.
        info!(issue = issue.number, "validating issue fix diff against guardrails");
        if let Err(violation) = guardrails::validate_diff(worktree_path, &[], 500, false).await {
            warn!(
                issue = issue.number,
                violation = %violation,
                "guardrail violation, discarding issue fix changes"
            );
            let _ = guardrails::discard_changes(worktree_path).await;
            fixed = false;
        }
    }

    if fixed {
        // 5. Commit, create branch, push, create PR.
        let branch_name = format!(
            "autoanneal/issue-{}-{}",
            issue.number,
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        );

        // Create branch and commit.
        let checkout = tokio::process::Command::new("git")
            .args(["checkout", "-b", &branch_name])
            .current_dir(worktree_path)
            .output()
            .await;

        let checkout_ok = checkout.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if !checkout_ok {
            let stderr = checkout
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_else(|e| e.to_string());
            warn!(issue = issue.number, branch = %branch_name, error = %stderr, "git checkout failed");
        }

        let add = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(worktree_path)
            .output()
            .await;

        let add_ok = add.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if !add_ok {
            let stderr = add
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_else(|e| e.to_string());
            warn!(issue = issue.number, error = %stderr, "git add failed");
        }

        let has_commit = if checkout_ok && add_ok {
            let commit_msg = format!("autoanneal: fix issue #{}\n\n{}", issue.number, summary);
            let commit = tokio::process::Command::new("git")
                .args(["commit", "-m", &commit_msg])
                .current_dir(worktree_path)
                .output()
                .await;

            let commit_ok = commit.as_ref().map(|o| o.status.success()).unwrap_or(false);
            if !commit_ok {
                let stderr = commit
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                    .unwrap_or_else(|e| e.to_string());
                warn!(issue = issue.number, error = %stderr, "git commit failed");
            }
            commit_ok
        } else {
            warn!(issue = issue.number, checkout_ok, add_ok, "skipping commit due to earlier git failures");
            false
        };

        if has_commit {
            // Push.
            let push = tokio::process::Command::new("git")
                .args(["push", "origin", &branch_name])
                .current_dir(worktree_path)
                .output()
                .await;

            if push.map(|o| o.status.success()).unwrap_or(false) {
                // Create PR.
                let pr_body = format!(
                    "Fixes #{}\n\n## Summary\n\n{}\n\n_Automated fix by autoanneal._",
                    issue.number, summary
                );
                let pr_title = format!("Fix #{}: {}", issue.number, issue.title);
                let pr_title = if pr_title.len() > 72 {
                    let truncated: String = pr_title.chars().take(69).collect();
                    format!("{}...", truncated)
                } else {
                    pr_title
                };

                match gh_command(
                    worktree_path,
                    &[
                        "pr",
                        "create",
                        "--draft",
                        "--title",
                        &pr_title,
                        "--body",
                        &pr_body,
                        "--head",
                        &branch_name,
                        "-R",
                        repo_slug,
                    ],
                )
                .await
                {
                    Ok(url) => {
                        let url = url.trim().to_string();
                        info!(pr_url = %url, issue = issue.number, "created draft PR for issue fix");
                        pr_url = Some(url.clone());

                        // Mark PR as ready for review.
                        let last_segment = url.rsplit('/').next().unwrap_or("");
                        if last_segment.is_empty() {
                            warn!(pr_url = %url, issue = issue.number, "PR URL has no trailing segment, cannot extract PR number");
                        } else if let Ok(pr_number) = last_segment.parse::<u64>() {
                            if let Err(e) = gh_command(
                                worktree_path,
                                &["pr", "ready", &pr_number.to_string(), "-R", repo_slug],
                            )
                            .await
                            {
                                warn!(error = %e, issue = issue.number, "failed to mark PR as ready (non-fatal)");
                            }
                        } else {
                            warn!(pr_url = %url, segment = %last_segment, issue = issue.number, "PR URL trailing segment is not a valid PR number");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, issue = issue.number, "failed to create PR for issue fix");
                    }
                }
            }
        }
    } else {
        // 6. Leave a comment with investigation findings.
        let comment = format!(
            "## Autoanneal Investigation\n\n{}\n\n_Automated investigation by autoanneal. Could not produce a fix automatically._",
            summary
        );
        let _ = gh_command(
            dot,
            &[
                "issue",
                "comment",
                &issue.number.to_string(),
                "--body",
                &comment,
                "-R",
                repo_slug,
            ],
        )
        .await;
    }

    // 7. Add autoanneal:attempted label, remove autoanneal:investigating.
    let _ = gh_command(
        dot,
        &[
            "label",
            "create",
            "autoanneal:attempted",
            "--color",
            "C2E0C6",
            "--force",
            "-R",
            repo_slug,
        ],
    )
    .await;

    let _ = gh_command(
        dot,
        &[
            "issue",
            "edit",
            &issue.number.to_string(),
            "--add-label",
            "autoanneal:attempted",
            "--remove-label",
            "autoanneal:investigating",
            "-R",
            repo_slug,
        ],
    )
    .await;

    info!(
        issue = issue.number,
        fixed,
        cost_usd,
        "issue investigation complete"
    );

    Ok(IssueOutput {
        issue_number: issue.number,
        fixed,
        pr_url,
        cost_usd,
    })
}
