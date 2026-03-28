use crate::models::{CiStatus, ExternalPr, GithubIssue, InFlightPr, RepoInfo};
use crate::retry::gh_command;
use anyhow::{bail, Context, Result};
use std::path::Path;
use tracing::{info, warn};

#[allow(dead_code)]
pub struct PreflightOutput {
    pub repo_info: RepoInfo,
    pub in_flight_prs: Vec<InFlightPr>,
    pub head_sha: String,
    /// Number of autoanneal runs since the last commit on the default branch.
    pub analysis_runs_since_last_commit: usize,
    /// Age of the newest commit across all branches (seconds), computed via API.
    pub newest_commit_age_secs: u64,
    /// External (non-autoanneal) PRs detected during preflight.
    pub external_prs: Vec<ExternalPr>,
    /// GitHub issues fetched for investigation.
    pub issues: Vec<GithubIssue>,
}

impl PreflightOutput {
    #[allow(dead_code)]
    pub fn prs_needing_ci_fix(&self) -> Vec<&InFlightPr> {
        self.in_flight_prs
            .iter()
            .filter(|pr| pr.ci_status == CiStatus::Failing && !pr.has_fixing_label)
            .collect()
    }

    #[allow(dead_code)]
    pub fn prs_needing_rebase(&self) -> Vec<&InFlightPr> {
        self.in_flight_prs
            .iter()
            .filter(|pr| pr.has_merge_conflicts && !pr.has_fixing_label)
            .collect()
    }
}

/// Validate environment and repo, return repo metadata plus in-flight PR info.
pub async fn run(repo_slug: &str, review_prs: bool, review_filter: &str, investigate_issues: &str) -> Result<PreflightOutput> {
    // 1. Validate environment variables.
    validate_env_vars()?;

    // 2. Validate GitHub auth by making a real API call.
    let dot = Path::new(".");

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

    // 6. Get HEAD SHA.
    let head_sha = get_head_sha(repo_slug, &default_branch).await;

    // 7. Detect external PRs if review is enabled.
    let external_prs = if review_prs {
        detect_external_prs(repo_slug, review_filter).await
    } else {
        Vec::new()
    };

    // 8. Fetch issues if investigation is enabled.
    let issues = if !investigate_issues.is_empty() {
        fetch_issues(repo_slug, investigate_issues).await
    } else {
        Vec::new()
    };

    // 9. Check staleness via API (no clone needed).
    let newest_commit_age = check_newest_commit_age_api(repo_slug).await;

    info!(
        in_flight = in_flight_prs.len(),
        external = external_prs.len(),
        issues = issues.len(),
        newest_commit_age_secs = newest_commit_age,
        "Preflight passed: {}/{}, default branch: {}",
        owner,
        name,
        default_branch
    );

    Ok(PreflightOutput {
        repo_info: info,
        in_flight_prs,
        head_sha,
        analysis_runs_since_last_commit: 0,
        newest_commit_age_secs: newest_commit_age,
        external_prs,
        issues,
    })
}

/// Fetch existing autoanneal/ branches and check for associated open PRs.
/// Returns best-effort results; failures are logged but not fatal.
async fn detect_in_flight_prs(repo_slug: &str) -> Vec<InFlightPr> {
    let dot = Path::new(".");
    let mut result = Vec::new();

    // List all remote branches (paginate first, then filter in Rust to avoid
    // missing branches that may be split across pages when using --jq with --paginate).
    let branches_raw = match gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/branches"),
            "--paginate",
        ],
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "failed to list branches (non-fatal)");
            return result;
        }
    };

    // Parse the JSON and filter for autoanneal/ branches in Rust.
    let branches: Vec<String> = match serde_json::from_str::<Vec<serde_json::Value>>(&branches_raw)
    {
        Ok(all_branches) => all_branches
            .into_iter()
            .filter_map(|b| b["name"].as_str().map(|s| s.to_string()))
            .filter(|name| name.starts_with("autoanneal/"))
            .collect(),
        Err(e) => {
            warn!(error = %e, "failed to parse branches JSON (non-fatal)");
            return result;
        }
    };

    let branches: Vec<&str> = branches.iter().map(|s| s.as_str()).collect();

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
                "number,title,body,mergeable,files",
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
                        // Check CI status
                        let ci_status = check_ci_status(repo_slug, number).await;

                        // Check for autoanneal:fixing label and stale detection
                        let mut has_fixing_label =
                            check_fixing_label(repo_slug, number).await;

                        // If label present but latest commit >30 min old, remove stale label
                        if has_fixing_label {
                            if is_stale_fixing(repo_slug, branch).await {
                                info!(
                                    pr_number = number,
                                    "removing stale autoanneal:fixing label"
                                );
                                let _ = gh_command(
                                    dot,
                                    &[
                                        "pr",
                                        "edit",
                                        &number.to_string(),
                                        "--remove-label",
                                        "autoanneal:fixing",
                                        "-R",
                                        repo_slug,
                                    ],
                                )
                                .await;
                                has_fixing_label = false;
                            }
                        }

                        let ci_status = if has_fixing_label {
                            CiStatus::Fixing
                        } else {
                            ci_status
                        };

                        // Check merge conflict status
                        let has_merge_conflicts = pr["mergeable"]
                            .as_str()
                            .map(|m| m == "CONFLICTING")
                            .unwrap_or(false);

                        // Extract changed file paths from PR.
                        let files: Vec<String> = pr["files"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|f| f["path"].as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();

                        result.push(InFlightPr {
                            number,
                            title,
                            body,
                            branch: branch.to_string(),
                            ci_status,
                            has_fixing_label,
                            has_merge_conflicts,
                            files,
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

/// Check CI status for a PR by inspecting check runs.
async fn check_ci_status(repo_slug: &str, pr_number: u64) -> CiStatus {
    let dot = Path::new(".");
    match gh_command(
        dot,
        &[
            "pr",
            "checks",
            &pr_number.to_string(),
            "--json",
            "name,state,bucket",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => {
            let checks: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return CiStatus::Pending,
            };
            if checks.is_empty() {
                return CiStatus::Pending;
            }
            let any_failing = checks.iter().any(|c| {
                let bucket = c["bucket"].as_str().unwrap_or("");
                bucket == "fail"
            });
            let all_complete = checks.iter().all(|c| {
                let bucket = c["bucket"].as_str().unwrap_or("");
                bucket == "pass" || bucket == "fail"  // not "pending"
            });
            if any_failing {
                CiStatus::Failing
            } else if all_complete {
                CiStatus::Passing
            } else {
                CiStatus::Pending
            }
        }
        Err(e) => {
            warn!(pr_number, error = %e, "failed to check CI status (non-fatal)");
            CiStatus::Pending
        }
    }
}

/// Check if a PR has the autoanneal:fixing label.
async fn check_fixing_label(repo_slug: &str, pr_number: u64) -> bool {
    let dot = Path::new(".");
    match gh_command(
        dot,
        &[
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "labels",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => {
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return false,
            };
            if let Some(labels) = v["labels"].as_array() {
                labels
                    .iter()
                    .any(|l| l["name"].as_str() == Some("autoanneal:fixing"))
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Check if the fixing label is stale (latest commit >30 min old).
async fn is_stale_fixing(repo_slug: &str, branch: &str) -> bool {
    let dot = Path::new(".");
    match gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/commits?sha={branch}&per_page=1"),
            "--jq",
            ".[0].commit.committer.date",
        ],
    )
    .await
    {
        Ok(raw) => {
            let date_str = raw.trim();
            // Parse ISO 8601 date
            if let Ok(commit_time) = chrono::DateTime::parse_from_rfc3339(date_str) {
                let age = chrono::Utc::now().signed_duration_since(commit_time);
                age.num_minutes() > 30
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Get the HEAD SHA of the default branch.
async fn get_head_sha(repo_slug: &str, default_branch: &str) -> String {
    let dot = Path::new(".");
    match gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/git/ref/heads/{default_branch}"),
            "--jq",
            ".object.sha",
        ],
    )
    .await
    {
        Ok(raw) => raw.trim().to_string(),
        Err(e) => {
            warn!(error = %e, "failed to get HEAD sha (non-fatal)");
            String::new()
        }
    }
}

/// Check newest commit age via GitHub API (no clone needed).
/// Checks the default branch and all autoanneal/ branches.
/// Returns age in seconds, or 0 if unable to determine (don't skip).
async fn check_newest_commit_age_api(repo_slug: &str) -> u64 {
    let dot = Path::new(".");

    // Get latest commit on default branch
    let result = gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/commits?per_page=1"),
            "--jq",
            ".[0].commit.committer.date",
        ],
    )
    .await;

    let mut newest_date: Option<chrono::DateTime<chrono::Utc>> = None;

    if let Ok(raw) = result {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
            newest_date = Some(dt.with_timezone(&chrono::Utc));
        }
    }

    // Also check autoanneal/ branches for recent commits — single API call;
    // the branches endpoint includes commit.commit.committer.date per branch,
    // so we avoid N+1 per-branch calls.
    let branches_result = gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/branches?per_page=100"),
            "--jq",
            r#"[.[] | select(.name | startswith("autoanneal/"))] | .[].commit.commit.committer.date"#,
        ],
    )
    .await;

    if let Ok(raw) = branches_result {
        for date_line in raw.lines().filter(|s| !s.is_empty() && s != "null") {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_line.trim()) {
                let dt_utc = dt.with_timezone(&chrono::Utc);
                if newest_date.map_or(true, |d| dt_utc > d) {
                    newest_date = Some(dt_utc);
                }
            }
        }
    }

    match newest_date {
        Some(dt) => {
            let age = chrono::Utc::now().signed_duration_since(dt);
            age.num_seconds().max(0) as u64
        }
        None => 0, // can't determine, don't skip
    }
}


/// Detect external (non-autoanneal) open PRs, filtered according to config.
async fn detect_external_prs(repo_slug: &str, filter: &str) -> Vec<ExternalPr> {
    let dot = Path::new(".");

    // 1. List all open PRs with relevant fields.
    let raw = match gh_command(
        dot,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,headRefName,author,updatedAt,labels",
            "--limit",
            "50",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "failed to list external PRs (non-fatal)");
            return Vec::new();
        }
    };

    let prs: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse external PR list JSON");
            return Vec::new();
        }
    };

    let mut result = Vec::new();

    for pr in prs {
        let branch = pr["headRefName"].as_str().unwrap_or("").to_string();

        // 2. Filter OUT autoanneal/ branches (those are ours).
        if branch.starts_with("autoanneal/") {
            continue;
        }

        // 3. Collect labels.
        let labels: Vec<String> = pr["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Filter OUT PRs already reviewed by autoanneal.
        if labels.iter().any(|l| l == "autoanneal:reviewed") {
            continue;
        }

        let number = pr["number"].as_u64().unwrap_or(0);
        if number == 0 {
            continue;
        }

        let title = pr["title"].as_str().unwrap_or("").to_string();
        let author = pr["author"]["login"].as_str().unwrap_or("").to_string();
        let updated_at = pr["updatedAt"].as_str().unwrap_or("").to_string();

        let external = ExternalPr {
            number,
            title,
            branch,
            author,
            updated_at,
            labels,
        };

        // 4. Apply configured filter.
        match filter {
            "all" => result.push(external),
            "recent" => {
                // Only keep PRs updated in the last 24 hours.
                if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&external.updated_at) {
                    let age = chrono::Utc::now().signed_duration_since(updated);
                    if age.num_hours() <= 24 {
                        result.push(external);
                    }
                }
            }
            f if f.starts_with("labeled:") => {
                let target_label = &f["labeled:".len()..];
                if external.labels.iter().any(|l| l == target_label) {
                    result.push(external);
                }
            }
            _ => {
                warn!(filter = %filter, "unknown review filter, treating as 'all'");
                result.push(external);
            }
        }
    }

    info!(count = result.len(), filter = %filter, "detected external PRs for review");
    result
}

/// Fetch open issues matching the given label filter.
/// Excludes issues already labeled autoanneal:investigating or autoanneal:attempted.
async fn fetch_issues(repo_slug: &str, label_filter: &str) -> Vec<GithubIssue> {
    let dot = Path::new(".");

    // Build label arg: comma-separated labels from the filter.
    let raw = match gh_command(
        dot,
        &[
            "issue",
            "list",
            "--label",
            label_filter,
            "--state",
            "open",
            "--json",
            "number,title,body,labels",
            "--limit",
            "20",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "failed to fetch issues (non-fatal)");
            return Vec::new();
        }
    };

    let issues: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse issue list JSON");
            return Vec::new();
        }
    };

    let mut result = Vec::new();
    for issue in issues {
        let labels: Vec<String> = issue["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Skip issues already being investigated or attempted.
        if labels
            .iter()
            .any(|l| l == "autoanneal:investigating" || l == "autoanneal:attempted")
        {
            continue;
        }

        let number = issue["number"].as_u64().unwrap_or(0);
        if number == 0 {
            continue;
        }

        result.push(GithubIssue {
            number,
            title: issue["title"].as_str().unwrap_or("").to_string(),
            body: issue["body"].as_str().unwrap_or("").to_string(),
            labels,
        });
    }

    info!(count = result.len(), label_filter = %label_filter, "fetched issues for investigation");
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
