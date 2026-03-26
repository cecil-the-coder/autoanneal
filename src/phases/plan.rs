use crate::claude::{invoke, ClaudeInvocation};
use crate::models::{Improvement, PrBody, RepoInfo};
use crate::prompts::plan::PR_BODY_PROMPT;
use crate::prompts::system::plan_system_prompt;
use crate::retry::gh_command;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

pub struct BranchOutput {
    pub branch_name: String,
}

pub struct PrOutput {
    pub pr_number: u64,
    pub pr_url: String,
    pub cost_usd: f64,
}

/// Create and push an empty branch as a lock.
/// No commits are created -- the branch simply points at the current HEAD.
pub async fn create_branch(
    clone_path: &Path,
    improvements: &[Improvement],
) -> Result<BranchOutput> {
    // 1. Generate branch name: autoanneal/{date}-{hash}
    let date = chrono::Utc::now().format("%Y%m%d");
    let improvements_json = serde_json::to_string(improvements)
        .context("failed to serialize improvements for hashing")?;
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(improvements_json.as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..3]) // first 3 bytes = 6 hex chars
    };
    let branch_name = format!("autoanneal/{date}-{hash}");

    // 2. Create local branch.
    let checkout_output = tokio::process::Command::new("git")
        .args(["checkout", "-b", &branch_name])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to spawn git checkout")?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        bail!("git checkout -b failed: {stderr}");
    }

    // 3. Push current HEAD as the remote branch (no commit needed).
    let push_output = tokio::process::Command::new("git")
        .args(["push", "origin", &format!("HEAD:refs/heads/{branch_name}")])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to spawn git push")?;

    if !push_output.status.success() {
        let stderr = String::from_utf8_lossy(&push_output.stderr);
        bail!("git push (lock branch) failed: {stderr}");
    }

    info!(branch = %branch_name, "lock branch created and pushed");

    Ok(BranchOutput { branch_name })
}

/// Create a draft PR after implementation has pushed real commits to the branch.
pub async fn create_pr(
    clone_path: &Path,
    repo_info: &RepoInfo,
    branch_name: &str,
    improvements: &[Improvement],
    model: &str,
    budget: f64,
) -> Result<PrOutput> {
    // 1. Generate PR body via Claude.
    let improvements_text = improvements
        .iter()
        .enumerate()
        .map(|(i, imp)| {
            format!(
                "{}. **{}** (severity: {:?}, category: {:?})\n   {}\n   Files: {}",
                i + 1,
                imp.title,
                imp.severity,
                imp.category,
                imp.description,
                imp.files_to_modify.join(", "),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let prompt = PR_BODY_PROMPT.replace("{improvements}", &improvements_text);

    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(plan_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 10,
        effort: "low",
        tools: "",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    let response = invoke::<PrBody>(&invocation, Duration::from_secs(120))
        .await
        .context("failed to generate PR body via Claude")?;

    let pr_body = response
        .structured
        .context("Claude did not return structured PR body output")?;

    let cost_usd = response.cost_usd;

    // 2. Create draft PR.
    let repo_slug = format!("{}/{}", repo_info.owner, repo_info.name);
    let pr_url_raw = gh_command(
        clone_path,
        &[
            "pr",
            "create",
            "--draft",
            "--title",
            &pr_body.title,
            "--body",
            &pr_body.body,
            "--head",
            branch_name,
            "-R",
            &repo_slug,
        ],
    )
    .await
    .context("failed to create draft PR")?;

    let pr_url = pr_url_raw.trim().to_string();

    // Extract PR number from URL (last path segment).
    let pr_number: u64 = pr_url
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .context("failed to extract PR number from URL")?;

    info!(pr_url = %pr_url, pr_number, "draft PR created");

    // 3. Mark PR as ready for review.
    let _ = gh_command(
        clone_path,
        &["pr", "ready", &pr_number.to_string(), "-R", &repo_slug],
    )
    .await
    .map_err(|e| warn!("failed to mark PR as ready (non-fatal): {e}"));

    Ok(PrOutput {
        pr_number,
        pr_url,
        cost_usd,
    })
}
