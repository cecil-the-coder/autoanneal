use crate::claude::{self, ClaudeInvocation, generate_session_id};
use crate::models::{GithubIssue, RepoInfo, StackInfo};
use crate::prompts;
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
    let session_id = generate_session_id();

    // 3. Invoke Claude.
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(system_prompt),
        model: model.to_string(),
        max_budget_usd: budget.min(3.0),
        max_turns: 100,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: worktree_path.to_path_buf(),
        session_id: Some(session_id),
        resume_session_id: None,
    };

    let response: claude::ClaudeResponse<serde_json::Value> =
        claude::invoke(&invocation, Duration::from_secs(900)).await?;

    let cost_usd = response.cost_usd;

    // 4. Parse the result to check if fixed.
    let fixed = response
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
        // 5. Commit, create branch, push, create PR.
        let branch_name = format!(
            "autoanneal/issue-{}-{}",
            issue.number,
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        );

        // Create branch and commit.
        let _ = tokio::process::Command::new("git")
            .args(["checkout", "-b", &branch_name])
            .current_dir(worktree_path)
            .output()
            .await;

        let _ = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(worktree_path)
            .output()
            .await;

        let commit_msg = format!("autoanneal: fix issue #{}\n\n{}", issue.number, summary);
        let commit = tokio::process::Command::new("git")
            .args(["commit", "-m", &commit_msg])
            .current_dir(worktree_path)
            .output()
            .await;

        let has_commit = commit.map(|o| o.status.success()).unwrap_or(false);

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
                    format!("{}...", &pr_title[..69])
                } else {
                    pr_title
                };

                match gh_command(
                    worktree_path,
                    &[
                        "pr",
                        "create",
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
                        info!(pr_url = %url, issue = issue.number, "created PR for issue fix");
                        pr_url = Some(url);
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
