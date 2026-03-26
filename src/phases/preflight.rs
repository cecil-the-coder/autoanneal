use crate::models::{InFlightPr, RepoInfo};
use crate::retry::gh_command;
use anyhow::{bail, Context, Result};
use std::path::Path;
use tracing::{info, warn};

pub struct PreflightOutput {
    pub repo_info: RepoInfo,
    pub in_flight_prs: Vec<InFlightPr>,
}

/// Validate environment and repo, return repo metadata plus in-flight PR info.
pub async fn run(repo_slug: &str) -> Result<PreflightOutput> {
    // 1. Validate environment variables.
    validate_env_vars()?;

    // 2. Validate GitHub auth.
    let dot = Path::new(".");
    gh_command(dot, &["auth", "status"])
        .await
        .context("GitHub CLI authentication check failed")?;

    // 3. Fetch repo metadata.
    let json_fields = "isArchived,defaultBranchRef,diskUsage,name,owner,viewerPermission";
    let raw = gh_command(dot, &["repo", "view", repo_slug, "--json", json_fields])
        .await
        .context("Failed to fetch repo metadata")?;

    let v: serde_json::Value =
        serde_json::from_str(&raw).context("Failed to parse repo metadata JSON")?;

    let is_archived = v["isArchived"].as_bool().unwrap_or(false);
    let default_branch = v["defaultBranchRef"]["name"]
        .as_str()
        .context("Missing defaultBranchRef.name in repo metadata")?
        .to_string();
    let disk_usage_kb = v["diskUsage"].as_u64().unwrap_or(0);
    let name = v["name"]
        .as_str()
        .context("Missing name in repo metadata")?
        .to_string();
    let owner = v["owner"]["login"]
        .as_str()
        .context("Missing owner.login in repo metadata")?
        .to_string();
    let viewer_permission = v["viewerPermission"]
        .as_str()
        .context("Missing viewerPermission in repo metadata")?
        .to_string();

    // 4. Validate repo state.
    if is_archived {
        bail!("Repository is archived");
    }

    if viewer_permission != "WRITE" && viewer_permission != "ADMIN" {
        bail!(
            "Insufficient permissions (need WRITE or ADMIN, got {viewer_permission})"
        );
    }

    let info = RepoInfo {
        owner: owner.clone(),
        name: name.clone(),
        default_branch: default_branch.clone(),
        disk_usage_kb,
        viewer_permission,
    };

    // 5. Detect in-flight autoanneal branches and their associated PRs.
    let in_flight_prs = detect_in_flight_prs(repo_slug).await;

    info!(
        in_flight = in_flight_prs.len(),
        "Preflight passed: {}/{}, default branch: {}",
        owner,
        name,
        default_branch
    );

    Ok(PreflightOutput {
        repo_info: info,
        in_flight_prs,
    })
}

/// Fetch existing autoanneal/ branches and check for associated open PRs.
/// Returns best-effort results; failures are logged but not fatal.
async fn detect_in_flight_prs(repo_slug: &str) -> Vec<InFlightPr> {
    let dot = Path::new(".");
    let mut result = Vec::new();

    // List remote branches matching autoanneal/*.
    let branches_raw = match gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/branches"),
            "--paginate",
            "--jq",
            r#".[].name | select(startswith("autoanneal/"))"#,
        ],
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "failed to list autoanneal branches (non-fatal)");
            return result;
        }
    };

    let branches: Vec<&str> = branches_raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if branches.is_empty() {
        return result;
    }

    info!(count = branches.len(), "found existing autoanneal branches");

    // For each branch, check for an associated open PR.
    for branch in branches {
        match gh_command(
            dot,
            &[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "open",
                "--json",
                "number,title,body",
                "--limit",
                "1",
                "-R",
                repo_slug,
            ],
        )
        .await
        {
            Ok(raw) => {
                let prs: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(pr) = prs.first() {
                    let number = pr["number"].as_u64().unwrap_or(0);
                    let title = pr["title"].as_str().unwrap_or("").to_string();
                    let body = pr["body"].as_str().unwrap_or("").to_string();
                    if number > 0 {
                        result.push(InFlightPr {
                            number,
                            title,
                            body,
                            branch: branch.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                warn!(branch = %branch, error = %e, "failed to check PR for branch (non-fatal)");
            }
        }
    }

    info!(count = result.len(), "found in-flight autoanneal PRs");
    result
}

/// Check that required environment variables are set and non-empty.
fn validate_env_vars() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        bail!("ANTHROPIC_API_KEY is not set or empty");
    }

    let gh_token = std::env::var("GH_TOKEN").unwrap_or_default();
    let github_token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if gh_token.is_empty() && github_token.is_empty() {
        bail!("Neither GH_TOKEN nor GITHUB_TOKEN is set");
    }

    Ok(())
}
