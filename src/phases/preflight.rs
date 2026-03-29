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

/// Configuration for PR fetching and filtering.
#[derive(Debug, Clone)]
pub struct PrFetchConfig {
    pub review_prs: bool,
    pub review_filter: String,
    pub fix_external_ci: bool,
    pub fix_conflicts: bool,
}

/// Validate environment and repo, return repo metadata plus in-flight PR info.
pub async fn run(
    repo_slug: &str,
    review_prs: bool,
    review_filter: &str,
    investigate_issues: &str,
    fix_external_ci: bool,
    fix_conflicts: bool,
) -> Result<PreflightOutput> {
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

    // 5. Fetch all open PRs in a single API call and partition them.
    let fetch_config = PrFetchConfig {
        review_prs,
        review_filter: review_filter.to_string(),
        fix_external_ci,
        fix_conflicts,
    };
    let needs_external = review_prs || fix_external_ci || fix_conflicts;
    let (in_flight_prs, external_prs) =
        fetch_all_prs(repo_slug, &fetch_config, needs_external).await;

    // 6. Get HEAD SHA.
    let head_sha = get_head_sha(repo_slug, &default_branch).await;

    // 7. Fetch issues if investigation is enabled.
    let issues = if !investigate_issues.is_empty() {
        fetch_issues(repo_slug, investigate_issues).await
    } else {
        Vec::new()
    };

    // 8. Check staleness via API (no clone needed).
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

/// Fetch all open PRs in a single API call, then partition into in-flight
/// (autoanneal/) and external PRs based on config-driven filtering.
///
/// The `needs_external` flag controls whether external PRs are processed at all.
/// When false, only in-flight PRs are returned and external PRs are empty.
async fn fetch_all_prs(
    repo_slug: &str,
    config: &PrFetchConfig,
    needs_external: bool,
) -> (Vec<InFlightPr>, Vec<ExternalPr>) {
    let dot = Path::new(".");

    // Single API call to get all open PRs with all needed fields.
    let prs_raw = match gh_command(
        dot,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,body,mergeable,files,headRefName,labels,author,updatedAt",
            "--limit",
            "500",
            "-R",
            repo_slug,
        ],
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "failed to list open PRs (non-fatal)");
            return (Vec::new(), Vec::new());
        }
    };

    let all_prs: Vec<serde_json::Value> = match serde_json::from_str(&prs_raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse PR list JSON (non-fatal)");
            return (Vec::new(), Vec::new());
        }
    };

    // Partition PRs by branch prefix.
    let (autoanneal_prs, other_prs): (Vec<_>, Vec<_>) = all_prs
        .iter()
        .partition(|pr| {
            pr["headRefName"]
                .as_str()
                .map(|s| s.starts_with("autoanneal/"))
                .unwrap_or(false)
        });

    // Process in-flight (autoanneal/) PRs.
    let mut in_flight = Vec::new();
    for pr in &autoanneal_prs {
        if let Some(ifp) = process_in_flight_pr(repo_slug, pr).await {
            in_flight.push(ifp);
        }
    }
    info!(count = in_flight.len(), "found in-flight autoanneal PRs");

    // Process external PRs based on config flags.
    let external = if needs_external {
        let filtered = filter_external_prs(&other_prs, config);
        // Only check CI status for PRs we actually care about.
        let mut result = Vec::new();
        for mut ext in filtered {
            let ci_status = check_ci_status(repo_slug, ext.number).await;
            ext.ci_status = ci_status;
            // Count autoanneal commits for PRs with failing CI so the
            // orchestrator can enforce the attempt limit.
            if ci_status == CiStatus::Failing {
                ext.autoanneal_commit_count =
                    count_autoanneal_commits(repo_slug, ext.number).await;
            }
            result.push(ext);
        }
        // Now do a second pass to drop PRs that were only included for CI reasons
        // but don't actually have failing CI. This avoids wasting CI check calls
        // on PRs we won't use, while still checking CI for review candidates.
        let final_result: Vec<ExternalPr> = result
            .into_iter()
            .filter(|pr| {
                let dominated_by_review = !pr.reviewed && config.review_prs;
                let included_for_conflicts = pr.has_merge_conflicts && config.fix_conflicts;
                let included_for_ci = pr.ci_status == CiStatus::Failing && config.fix_external_ci;
                dominated_by_review || included_for_conflicts || included_for_ci
            })
            .collect();
        info!(
            count = final_result.len(),
            review_filter = %config.review_filter,
            "detected external PRs"
        );
        final_result
    } else {
        Vec::new()
    };

    (in_flight, external)
}

/// Process a single autoanneal/ PR into an InFlightPr.
async fn process_in_flight_pr(
    repo_slug: &str,
    pr: &serde_json::Value,
) -> Option<InFlightPr> {
    let dot = Path::new(".");
    let branch = pr["headRefName"].as_str()?.to_string();
    let number = extract_nonzero_number(pr, "In-flight PR")?;
    let title = pr["title"].as_str().unwrap_or("").to_string();
    let body = pr["body"].as_str().unwrap_or("").to_string();

    // Check CI status (per-PR, unavoidable).
    let ci_status = check_ci_status(repo_slug, number).await;

    // Check for autoanneal:fixing label from already-fetched PR data.
    let mut has_fixing_label = pr["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|l| l["name"].as_str() == Some("autoanneal:fixing"))
        })
        .unwrap_or(false);

    // If label present but latest commit >30 min old, remove stale label.
    if has_fixing_label {
        let stale = is_stale_fixing(repo_slug, &branch).await;

        if stale {
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

    // Check merge conflict status.
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

    Some(InFlightPr {
        number,
        title,
        body,
        branch,
        ci_status,
        has_fixing_label,
        has_merge_conflicts,
        files,
    })
}

/// Filter external PRs based on config flags.
///
/// Inclusion rules:
/// - If `review_prs`: include unreviewed PRs (no `autoanneal:reviewed` label),
///   applying the `review_filter`.
/// - If `fix_external_ci`: include all non-autoanneal PRs (CI status checked later).
/// - If `fix_conflicts`: include PRs with `CONFLICTING` mergeable status.
///
/// PRs are deduplicated by number.
fn filter_external_prs(
    prs: &[&serde_json::Value],
    config: &PrFetchConfig,
) -> Vec<ExternalPr> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for pr in prs {
        let number = match extract_nonzero_number(pr, "External PR") {
            Some(n) => n,
            None => continue,
        };

        // Skip duplicates.
        if !seen.insert(number) {
            continue;
        }

        let branch = pr["headRefName"].as_str().unwrap_or("").to_string();
        let title = pr["title"].as_str().unwrap_or("").to_string();
        let author = pr["author"]["login"].as_str().unwrap_or("").to_string();
        let updated_at = pr["updatedAt"].as_str().unwrap_or("").to_string();
        let labels: Vec<String> = pr["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let reviewed = labels.iter().any(|l| l == "autoanneal:reviewed");
        let has_merge_conflicts = pr["mergeable"]
            .as_str()
            .map(|m| m == "CONFLICTING")
            .unwrap_or(false);

        // Determine if this PR should be included based on config flags.
        let include_for_review = config.review_prs
            && !reviewed
            && matches_review_filter(&config.review_filter, &updated_at, &labels);
        let include_for_conflicts = config.fix_conflicts && has_merge_conflicts;
        // For CI, we include all candidates and filter after CI status is checked.
        let include_for_ci = config.fix_external_ci;

        if !include_for_review && !include_for_conflicts && !include_for_ci {
            continue;
        }

        result.push(ExternalPr {
            number,
            title,
            branch,
            author,
            updated_at,
            labels,
            ci_status: CiStatus::Pending, // Will be filled after CI check.
            reviewed,
            autoanneal_commit_count: 0, // Filled after CI check for failing PRs.
            has_merge_conflicts,
        });
    }

    result
}

/// Check if a PR matches the review filter.
fn matches_review_filter(filter: &str, updated_at: &str, labels: &[String]) -> bool {
    match filter {
        "all" => true,
        "recent" => {
            if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(updated_at) {
                let age = chrono::Utc::now().signed_duration_since(updated);
                age.num_hours() <= 24
            } else {
                false
            }
        }
        f if f.starts_with("labeled:") => {
            let target_label = &f["labeled:".len()..];
            labels.iter().any(|l| l == target_label)
        }
        _ => {
            warn!(filter = %filter, "unknown review filter, treating as 'all'");
            true
        }
    }
}

/// Extract a non-zero number from a JSON object's "number" field.
/// Returns `None` and logs a warning if the field is missing, not a valid u64,
/// or equals zero — all indicating a malformed API response.
fn extract_nonzero_number(obj: &serde_json::Value, entity_type: &str) -> Option<u64> {
    match obj.get("number") {
        None => {
            warn!(entity_type, "has no 'number' field (malformed API response)");
            None
        }
        Some(v) => {
            if let Some(n) = v.as_u64() {
                if n == 0 {
                    warn!(entity_type, "number is 0 (malformed API response)");
                    None
                } else {
                    Some(n)
                }
            } else {
                warn!(entity_type, value = %v, "number is not a valid integer (malformed API response)");
                None
            }
        }
    }
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

/// Count commits on a PR whose message starts with "autoanneal:".
async fn count_autoanneal_commits(repo_slug: &str, pr_number: u64) -> u64 {
    let dot = Path::new(".");
    match gh_command(
        dot,
        &[
            "api",
            &format!("repos/{repo_slug}/pulls/{pr_number}/commits"),
            "--paginate",
            "--jq",
            r#"[.[] | select(.commit.message | startswith("autoanneal:"))] | length"#,
        ],
    )
    .await
    {
        Ok(raw) => {
            // With --paginate and --jq, each page emits a number on its own
            // line.  Sum them to get the total across all pages.
            raw.lines()
                .filter_map(|line| line.trim().parse::<u64>().ok())
                .sum()
        }
        Err(e) => {
            warn!(pr_number, error = %e, "failed to count autoanneal commits (non-fatal)");
            0
        }
    }
}

/// Check if a PR has the autoanneal:fixing label.
#[allow(dead_code)]
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
        for date_line in raw.lines().filter(|s| !s.is_empty() && *s != "null") {
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

        let number = match extract_nonzero_number(&issue, "Issue") {
            Some(n) => n,
            None => continue,
        };

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

#[cfg(test)]
mod tests {
    use super::*;


    /// Helper to build a mock PR JSON value.
    fn mock_pr(
        number: u64,
        branch: &str,
        labels: &[&str],
        mergeable: &str,
        updated_at: &str,
        author: &str,
    ) -> serde_json::Value {
        let label_arr: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| serde_json::json!({"name": l}))
            .collect();
        serde_json::json!({
            "number": number,
            "title": format!("PR #{number}"),
            "body": "",
            "headRefName": branch,
            "labels": label_arr,
            "mergeable": mergeable,
            "updatedAt": updated_at,
            "author": {"login": author},
            "files": [],
        })
    }

    #[test]
    fn test_partition_prs_by_branch() {
        let autoanneal_pr = mock_pr(1, "autoanneal/fix-typo", &[], "MERGEABLE", "", "bot");
        let external_pr = mock_pr(2, "feature/add-login", &[], "MERGEABLE", "", "alice");

        let all_prs = vec![autoanneal_pr, external_pr];
        let (autoanneal, other): (Vec<_>, Vec<_>) = all_prs
            .iter()
            .partition(|pr| {
                pr["headRefName"]
                    .as_str()
                    .map(|s| s.starts_with("autoanneal/"))
                    .unwrap_or(false)
            });

        assert_eq!(autoanneal.len(), 1);
        assert_eq!(other.len(), 1);
        assert_eq!(autoanneal[0]["number"], 1);
        assert_eq!(other[0]["number"], 2);
    }

    #[test]
    fn test_external_filter_review_only() {
        let unreviewed = mock_pr(1, "feat/a", &[], "MERGEABLE", "2026-03-29T00:00:00Z", "alice");
        let reviewed = mock_pr(2, "feat/b", &["autoanneal:reviewed"], "MERGEABLE", "2026-03-29T00:00:00Z", "bob");
        let prs: Vec<&serde_json::Value> = vec![&unreviewed, &reviewed];

        let config = PrFetchConfig {
            review_prs: true,
            review_filter: "all".to_string(),
            fix_external_ci: false,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 1);
        assert!(!result[0].reviewed);
    }

    #[test]
    fn test_external_filter_conflicts() {
        let reviewed_conflicting = mock_pr(
            1, "feat/a", &["autoanneal:reviewed"], "CONFLICTING", "2026-03-29T00:00:00Z", "alice",
        );
        let reviewed_clean = mock_pr(
            2, "feat/b", &["autoanneal:reviewed"], "MERGEABLE", "2026-03-29T00:00:00Z", "bob",
        );
        let prs: Vec<&serde_json::Value> = vec![&reviewed_conflicting, &reviewed_clean];

        let config = PrFetchConfig {
            review_prs: false,
            review_filter: "all".to_string(),
            fix_external_ci: false,
            fix_conflicts: true,
        };

        let result = filter_external_prs(&prs, &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 1);
        assert!(result[0].has_merge_conflicts);
        assert!(result[0].reviewed);
    }

    #[test]
    fn test_external_filter_ci() {
        // fix_external_ci includes all PRs as candidates (CI checked later).
        let reviewed = mock_pr(1, "feat/a", &["autoanneal:reviewed"], "MERGEABLE", "2026-03-29T00:00:00Z", "alice");
        let unreviewed = mock_pr(2, "feat/b", &[], "MERGEABLE", "2026-03-29T00:00:00Z", "bob");
        let prs: Vec<&serde_json::Value> = vec![&reviewed, &unreviewed];

        let config = PrFetchConfig {
            review_prs: false,
            review_filter: "all".to_string(),
            fix_external_ci: true,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        // Both included because fix_external_ci includes all as CI candidates.
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_external_filter_combined() {
        let pr_review = mock_pr(1, "feat/a", &[], "MERGEABLE", "2026-03-29T00:00:00Z", "alice");
        let pr_conflict = mock_pr(2, "feat/b", &["autoanneal:reviewed"], "CONFLICTING", "2026-03-29T00:00:00Z", "bob");
        let pr_ci = mock_pr(3, "feat/c", &["autoanneal:reviewed"], "MERGEABLE", "2026-03-29T00:00:00Z", "carol");
        let prs: Vec<&serde_json::Value> = vec![&pr_review, &pr_conflict, &pr_ci];

        let config = PrFetchConfig {
            review_prs: true,
            review_filter: "all".to_string(),
            fix_external_ci: true,
            fix_conflicts: true,
        };

        let result = filter_external_prs(&prs, &config);
        // All 3 included: #1 for review, #2 for conflicts, #3 for CI candidate.
        assert_eq!(result.len(), 3);
        // No duplicates.
        let numbers: Vec<u64> = result.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn test_external_filter_nothing_enabled() {
        let pr = mock_pr(1, "feat/a", &[], "MERGEABLE", "2026-03-29T00:00:00Z", "alice");
        let prs: Vec<&serde_json::Value> = vec![&pr];

        let config = PrFetchConfig {
            review_prs: false,
            review_filter: "all".to_string(),
            fix_external_ci: false,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        assert!(result.is_empty());
    }

    #[test]
    fn test_reviewed_label_detection() {
        let reviewed = mock_pr(1, "feat/a", &["autoanneal:reviewed"], "MERGEABLE", "", "alice");
        let not_reviewed = mock_pr(2, "feat/b", &["bug", "enhancement"], "MERGEABLE", "", "bob");
        let no_labels = mock_pr(3, "feat/c", &[], "MERGEABLE", "", "carol");
        let prs: Vec<&serde_json::Value> = vec![&reviewed, &not_reviewed, &no_labels];

        let config = PrFetchConfig {
            review_prs: true,
            review_filter: "all".to_string(),
            fix_external_ci: false,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        // Only unreviewed PRs included for review.
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|pr| !pr.reviewed));
        assert!(result.iter().any(|pr| pr.number == 2));
        assert!(result.iter().any(|pr| pr.number == 3));
    }

    #[test]
    fn test_review_filter_recent() {
        let recent = mock_pr(1, "feat/a", &[], "MERGEABLE", "2026-03-29T00:00:00Z", "alice");
        let old = mock_pr(2, "feat/b", &[], "MERGEABLE", "2025-01-01T00:00:00Z", "bob");
        let prs: Vec<&serde_json::Value> = vec![&recent, &old];

        let config = PrFetchConfig {
            review_prs: true,
            review_filter: "recent".to_string(),
            fix_external_ci: false,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        // Only the recent PR should be included (the old one is >24h old).
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 1);
    }

    #[test]
    fn test_review_filter_labeled() {
        let labeled = mock_pr(1, "feat/a", &["needs-review"], "MERGEABLE", "", "alice");
        let unlabeled = mock_pr(2, "feat/b", &["bug"], "MERGEABLE", "", "bob");
        let prs: Vec<&serde_json::Value> = vec![&labeled, &unlabeled];

        let config = PrFetchConfig {
            review_prs: true,
            review_filter: "labeled:needs-review".to_string(),
            fix_external_ci: false,
            fix_conflicts: false,
        };

        let result = filter_external_prs(&prs, &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].number, 1);
    }

    #[test]
    fn test_conflicts_includes_reviewed_prs() {
        // A reviewed PR with conflicts should still be included when fix_conflicts is enabled.
        let reviewed_conflicting = mock_pr(
            1, "feat/a", &["autoanneal:reviewed"], "CONFLICTING", "", "alice",
        );
        let prs: Vec<&serde_json::Value> = vec![&reviewed_conflicting];

        let config = PrFetchConfig {
            review_prs: false,
            review_filter: "all".to_string(),
            fix_external_ci: false,
            fix_conflicts: true,
        };

        let result = filter_external_prs(&prs, &config);
        assert_eq!(result.len(), 1);
        assert!(result[0].reviewed);
        assert!(result[0].has_merge_conflicts);
    }
}
