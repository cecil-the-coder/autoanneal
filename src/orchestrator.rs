use crate::cleanup::CleanupGuard;
use crate::config::Config;
use crate::logging;
use crate::models::{
    ExternalPr, GithubIssue, InFlightPr, OpenPr, PhaseReport, RepoInfo, StackInfo,
    TaskStatus,
};
use crate::phases;
use crate::worktree::WorktreeManager;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Work-queue types
// ---------------------------------------------------------------------------

/// A single unit of concurrent work.
struct WorkItem {
    kind: WorkItemKind,
    budget_cap: f64,
}

enum WorkItemKind {
    CiFix {
        pr: InFlightPr,
    },
    PrReview {
        pr: ExternalPr,
        fix_threshold: u32,
    },
    IssueInvestigation {
        issue: GithubIssue,
        repo_info: RepoInfo,
        arch_summary: String,
        stack_info: StackInfo,
    },
    Analysis {
        clone_path: PathBuf,
        repo_info: RepoInfo,
        arch_summary: String,
        stack_info: StackInfo,
        open_prs: Vec<OpenPr>,
        model: String,
        max_tasks: usize,
        min_severity: crate::models::Severity,
        improve_docs: bool,
        dry_run: bool,
        critic_threshold: u32,
        doc_critic_threshold: u32,
    },
}

impl WorkItem {
    fn name(&self) -> String {
        match &self.kind {
            WorkItemKind::CiFix { pr } => format!("CI Fix (PR #{})", pr.number),
            WorkItemKind::PrReview { pr, .. } => format!("PR Review (PR #{})", pr.number),
            WorkItemKind::IssueInvestigation { issue, .. } => {
                format!("Issue #{}", issue.number)
            }
            WorkItemKind::Analysis { .. } => "Analysis Pipeline".to_string(),
        }
    }
}

/// Outcome of a single work item.
struct WorkItemOutcome {
    item_name: String,
    result: Result<WorkItemResult>,
    cost_usd: f64,
    duration: Duration,
}

#[allow(dead_code)]
enum WorkItemResult {
    CiFix {
        pr_number: u64,
        fixed: bool,
    },
    PrReview {
        pr_number: u64,
        score: u32,
        fixed: bool,
        commented: bool,
    },
    IssueInvestigation {
        issue_number: u64,
        fixed: bool,
        pr_url: Option<String>,
    },
    AnalysisPipeline {
        pr_url: Option<String>,
        branch_name: Option<String>,
        pr_number: Option<u64>,
        has_successful_tasks: bool,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full autoworker pipeline. Returns exit code (0, 1, or 2).
pub async fn run(config: &Config) -> Result<i32> {
    // --- Setup ---
    let repo_slug = config.repo_slug();
    let timeout_duration = config.timeout_duration();
    let min_severity = config.min_severity();
    let mut budget_remaining = config.max_budget;
    let mut phases_report: Vec<PhaseReport> = Vec::new();
    let mut total_cost: f64 = 0.0;

    // Create work directory.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let work_dir = PathBuf::from(format!("/tmp/autoanneal-{timestamp}"));
    std::fs::create_dir_all(&work_dir)
        .with_context(|| format!("failed to create work directory: {}", work_dir.display()))?;

    info!(work_dir = %work_dir.display(), "created work directory");

    // Create cleanup guard (repo_dir will be updated after clone).
    let mut cleanup_guard = CleanupGuard::new(
        work_dir.clone(),
        repo_slug.clone(),
        config.keep_on_failure,
    );

    // Wrap the entire pipeline in the global timeout.
    let result = tokio::time::timeout(timeout_duration, async {
        run_pipeline(
            config,
            &repo_slug,
            &min_severity,
            &work_dir,
            &mut budget_remaining,
            &mut phases_report,
            &mut total_cost,
            &mut cleanup_guard,
        )
        .await
    })
    .await;

    match result {
        Ok(outcome) => {
            let exit_code = outcome?;

            // Print summary on all paths.
            logging::print_summary(
                &repo_slug,
                cleanup_guard.branch_name.as_deref(),
                None, // PR URL printed separately below
                &phases_report,
                total_cost,
            );

            Ok(exit_code)
        }
        Err(_elapsed) => {
            // Global timeout fired.
            warn!("global timeout reached ({timeout_duration:?})");

            phases_report.push(PhaseReport {
                name: "Timeout".to_string(),
                duration: Duration::ZERO,
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });

            logging::print_summary(
                &repo_slug,
                cleanup_guard.branch_name.as_deref(),
                None,
                &phases_report,
                total_cost,
            );

            // Cleanup guard will fire on drop (unless disarmed).
            Ok(2)
        }
    }
}

/// Inner pipeline, separated so it can be wrapped in a global timeout.
async fn run_pipeline(
    config: &Config,
    repo_slug: &str,
    min_severity: &crate::models::Severity,
    work_dir: &PathBuf,
    budget_remaining: &mut f64,
    phases_report: &mut Vec<PhaseReport>,
    total_cost: &mut f64,
    cleanup_guard: &mut CleanupGuard,
) -> Result<i32> {
    // ─── Phase 1: Preflight ─────────────────────────────────────────────
    info!("starting phase: Preflight");
    let phase_start = Instant::now();

    let preflight_output = match tokio::time::timeout(
        Duration::from_secs(60),
        phases::preflight::run(
            repo_slug,
            config.review_prs,
            &config.review_filter,
            &config.investigate_issues,
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            phases_report.push(PhaseReport {
                name: "Preflight".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "OK".to_string(),
            });
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "preflight failed");
            phases_report.push(PhaseReport {
                name: "Preflight".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            return Ok(1);
        }
        Err(_) => {
            warn!("preflight timed out");
            phases_report.push(PhaseReport {
                name: "Preflight".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    // ─── Early exit checks (before recon to save money) ────────────────
    let has_maintenance = !preflight_output.prs_needing_ci_fix().is_empty()
        || !preflight_output.prs_needing_rebase().is_empty();
    let has_reviews = !preflight_output.external_prs.is_empty();
    let has_issues = !preflight_output.issues.is_empty();
    let at_pr_limit = config.max_open_prs > 0
        && preflight_output.in_flight_prs.len() >= config.max_open_prs;

    // If at PR limit and no maintenance/review/issue work, skip everything.
    if at_pr_limit && !has_maintenance && !has_reviews && !has_issues {
        info!(
            open = preflight_output.in_flight_prs.len(),
            max = config.max_open_prs,
            "at max open PRs with no maintenance work, skipping"
        );
        println!("At max open PRs ({}/{}). No maintenance work. Skipping.",
            preflight_output.in_flight_prs.len(), config.max_open_prs);
        phases_report.push(PhaseReport {
            name: "Skip".to_string(),
            duration: Duration::ZERO,
            cost_usd: 0.0,
            status: format!("SKIPPED (max open PRs: {})", config.max_open_prs),
        });
        return Ok(0);
    }

    let has_work = has_maintenance || has_reviews || has_issues;

    if config.skip_after > 0 && !has_work {
        let threshold_secs = config.skip_after as u64 * config.cron_interval * 60;
        if preflight_output.newest_commit_age_secs > threshold_secs {
            info!(
                age_secs = preflight_output.newest_commit_age_secs,
                threshold_secs,
                "no recent commits on any branch, skipping"
            );
            println!(
                "No recent commits (newest is {}s old, threshold {}s). Skipping.",
                preflight_output.newest_commit_age_secs, threshold_secs
            );
            phases_report.push(PhaseReport {
                name: "Skip".to_string(),
                duration: Duration::ZERO,
                cost_usd: 0.0,
                status: format!("SKIPPED (no commits in {}m)", threshold_secs / 60),
            });
            return Ok(0);
        }
    }

    let repo_info = preflight_output.repo_info;
    let in_flight_prs = preflight_output.in_flight_prs;

    // ─── Phase 2: Recon ─────────────────────────────────────────────────
    if *budget_remaining <= 0.0 {
        warn!("budget exhausted before Recon phase");
        return Ok(2);
    }

    info!("starting phase: Recon");
    let phase_start = Instant::now();
    let recon_budget = config.max_budget * 0.05;

    let recon_output = match tokio::time::timeout(
        Duration::from_secs(300),
        phases::recon::run(
            &repo_info,
            work_dir,
            &config.model,
            recon_budget,
            config.setup_command.as_deref(),
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            let cost = output.cost_usd;
            *budget_remaining -= cost;
            *total_cost += cost;
            phases_report.push(PhaseReport {
                name: "Recon".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: cost,
                status: "OK".to_string(),
            });
            // Update cleanup guard with the actual clone path.
            cleanup_guard.repo_dir = output.clone_path.clone();
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "recon failed");
            phases_report.push(PhaseReport {
                name: "Recon".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            return Ok(1);
        }
        Err(_) => {
            warn!("recon timed out");
            phases_report.push(PhaseReport {
                name: "Recon".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    let clone_path = recon_output.clone_path.clone();
    let stack_info = recon_output.stack_info.clone();
    let arch_summary = recon_output.arch_summary.clone();

    // Staleness check already done before recon (in preflight).

    // ─── Create worktree manager ─────────────────────────────────────────
    let worktree_mgr = Arc::new(WorktreeManager::new(clone_path.clone()));

    // ─── Build work queue ────────────────────────────────────────────────
    let work_items = collect_work_items(
        config,
        &preflight_output.external_prs,
        &in_flight_prs,
        &preflight_output.issues,
        &clone_path,
        &repo_info,
        &arch_summary,
        &stack_info,
        &recon_output.open_prs,
        min_severity,
        *budget_remaining,
    );

    if work_items.is_empty() {
        info!("no work items to process");
        println!("No actionable work items found.");
        return Ok(0);
    }

    info!(count = work_items.len(), "built work queue");

    // ─── Execute work queue concurrently ─────────────────────────────────
    let outcomes = run_work_queue(
        config.concurrency,
        work_items,
        worktree_mgr,
        repo_slug,
        &config.model,
    )
    .await;

    // ─── Process outcomes ────────────────────────────────────────────────
    let mut exit_code = 0;

    for outcome in &outcomes {
        *total_cost += outcome.cost_usd;
        *budget_remaining -= outcome.cost_usd;

        let status = match &outcome.result {
            Ok(r) => match r {
                WorkItemResult::CiFix { fixed, .. } => {
                    if *fixed {
                        "OK".to_string()
                    } else {
                        "NO_CHANGES".to_string()
                    }
                }
                WorkItemResult::PrReview {
                    fixed, commented, score, ..
                } => {
                    if *fixed {
                        "FIXED".to_string()
                    } else if *commented {
                        "COMMENTED".to_string()
                    } else {
                        format!("OK (score: {score})")
                    }
                }
                WorkItemResult::IssueInvestigation {
                    fixed, pr_url, ..
                } => {
                    if *fixed {
                        format!(
                            "FIXED (PR: {})",
                            pr_url.as_deref().unwrap_or("unknown")
                        )
                    } else {
                        "INVESTIGATED".to_string()
                    }
                }
                WorkItemResult::AnalysisPipeline {
                    pr_url,
                    branch_name,
                    pr_number,
                    has_successful_tasks,
                } => {
                    if let Some(url) = pr_url {
                        println!("PR: {url}");
                        // Update cleanup guard.
                        cleanup_guard.branch_name = branch_name.clone();
                        cleanup_guard.pr_number = *pr_number;
                        cleanup_guard.has_successful_tasks = *has_successful_tasks;
                        cleanup_guard.disarm();
                        "OK".to_string()
                    } else if *has_successful_tasks {
                        cleanup_guard.branch_name = branch_name.clone();
                        cleanup_guard.has_successful_tasks = true;
                        exit_code = 1;
                        "PARTIAL (no PR)".to_string()
                    } else {
                        cleanup_guard.branch_name = branch_name.clone();
                        exit_code = 1;
                        "FAILED".to_string()
                    }
                }
            },
            Err(e) => {
                warn!(item = %outcome.item_name, error = %e, "work item failed");
                format!("FAILED: {e}")
            }
        };

        phases_report.push(PhaseReport {
            name: outcome.item_name.clone(),
            duration: outcome.duration,
            cost_usd: outcome.cost_usd,
            status,
        });
    }

    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Work queue collection
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn collect_work_items(
    config: &Config,
    external_prs: &[ExternalPr],
    in_flight_prs: &[InFlightPr],
    issues: &[GithubIssue],
    clone_path: &PathBuf,
    repo_info: &RepoInfo,
    arch_summary: &str,
    stack_info: &StackInfo,
    open_prs: &[OpenPr],
    min_severity: &crate::models::Severity,
    budget_remaining: f64,
) -> Vec<WorkItem> {
    let mut items = Vec::new();

    // CI fix items.
    if config.fix_ci || config.fix_conflicts {
        let mut prs_to_fix: Vec<&InFlightPr> = Vec::new();
        if config.fix_ci {
            prs_to_fix.extend(
                in_flight_prs
                    .iter()
                    .filter(|pr| {
                        pr.ci_status == crate::models::CiStatus::Failing && !pr.has_fixing_label
                    }),
            );
        }
        if config.fix_conflicts {
            for pr in in_flight_prs.iter().filter(|pr| pr.has_merge_conflicts && !pr.has_fixing_label) {
                if !prs_to_fix.iter().any(|p| p.number == pr.number) {
                    prs_to_fix.push(pr);
                }
            }
        }
        for pr in prs_to_fix {
            let fix_budget = budget_remaining.min(2.0);
            if fix_budget <= 0.0 {
                break;
            }
            // Note: we don't reserve budget here; actual costs are subtracted
            // when outcomes are processed to avoid double-counting.
            items.push(WorkItem {
                kind: WorkItemKind::CiFix { pr: pr.clone() },
                budget_cap: fix_budget,
            });
        }
    }

    // PR review items.
    if config.review_prs {
        for pr in external_prs.iter().take(3) {
            let review_budget = budget_remaining.min(2.0);
            if review_budget <= 0.5 {
                break;
            }
            // Note: we don't reserve budget here; actual costs are subtracted
            // when outcomes are processed to avoid double-counting.
            items.push(WorkItem {
                kind: WorkItemKind::PrReview {
                    pr: pr.clone(),
                    fix_threshold: config.review_fix_threshold,
                },
                budget_cap: review_budget,
            });
        }
    }

    // Issue investigation items.
    for issue in issues.iter().take(config.max_issues) {
        let issue_budget = budget_remaining.min(config.issue_budget);
        if issue_budget <= 0.0 {
            break;
        }
        // Note: we don't reserve budget here; actual costs are subtracted
        // when outcomes are processed to avoid double-counting.
        items.push(WorkItem {
            kind: WorkItemKind::IssueInvestigation {
                issue: issue.clone(),
                repo_info: repo_info.clone(),
                arch_summary: arch_summary.to_string(),
                stack_info: stack_info.clone(),
            },
            budget_cap: issue_budget,
        });
    }

    // Merge open PRs with in-flight autoanneal PRs for analysis overlap avoidance.
    let mut merged_open_prs: Vec<OpenPr> = open_prs.to_vec();
    for ifp in in_flight_prs {
        // Extract file paths from the PR's files field.
        let files = ifp.files.iter().map(|f| f.clone()).collect();
        merged_open_prs.push(OpenPr {
            number: ifp.number,
            title: ifp.title.clone(),
            head_ref: ifp.branch.clone(),
            files,
        });
    }

    // Analysis pipeline item — skip if too many open autoanneal PRs already.
    let open_autoanneal_count = in_flight_prs.len();
    let skip_analysis = config.max_open_prs > 0 && open_autoanneal_count >= config.max_open_prs;
    if skip_analysis {
        info!(
            open = open_autoanneal_count,
            max = config.max_open_prs,
            "skipping analysis — too many open autoanneal PRs"
        );
    }
    if budget_remaining > 0.0 && !config.dry_run && !skip_analysis {
        let analysis_budget = budget_remaining; // analysis gets remaining budget
        // Note: we don't reserve budget here; actual costs are subtracted
        // when outcomes are processed to avoid double-counting.
        items.push(WorkItem {
            kind: WorkItemKind::Analysis {
                clone_path: clone_path.clone(),
                repo_info: repo_info.clone(),
                arch_summary: arch_summary.to_string(),
                stack_info: stack_info.clone(),
                open_prs: merged_open_prs,
                model: config.model.clone(),
                max_tasks: config.max_tasks,
                min_severity: *min_severity,
                improve_docs: config.improve_docs,
                dry_run: config.dry_run,
                critic_threshold: config.critic_threshold,
                doc_critic_threshold: config.doc_critic_threshold,
            },
            budget_cap: analysis_budget,
        });
    } else if config.dry_run && budget_remaining > 0.0 {
        // For dry-run, still run analysis but it will just print and return.
        let analysis_budget = budget_remaining;
        // Note: we don't reserve budget here; actual costs are subtracted
        // when outcomes are processed to avoid double-counting.
        items.push(WorkItem {
            kind: WorkItemKind::Analysis {
                clone_path: clone_path.clone(),
                repo_info: repo_info.clone(),
                arch_summary: arch_summary.to_string(),
                stack_info: stack_info.clone(),
                open_prs: merged_open_prs,
                model: config.model.clone(),
                max_tasks: config.max_tasks,
                min_severity: *min_severity,
                improve_docs: config.improve_docs,
                dry_run: config.dry_run,
                critic_threshold: config.critic_threshold,
                doc_critic_threshold: config.doc_critic_threshold,
            },
            budget_cap: analysis_budget,
        });
    }

    items
}

// ---------------------------------------------------------------------------
// Work queue execution
// ---------------------------------------------------------------------------

async fn run_work_queue(
    concurrency: usize,
    items: Vec<WorkItem>,
    worktree_mgr: Arc<WorktreeManager>,
    repo_slug: &str,
    model: &str,
) -> Vec<WorkItemOutcome> {
    let concurrency = concurrency.max(1);
    let mut pending: VecDeque<WorkItem> = items.into();
    let mut join_set: JoinSet<WorkItemOutcome> = JoinSet::new();
    let mut outcomes: Vec<WorkItemOutcome> = Vec::new();

    // Fill initial slots.
    while join_set.len() < concurrency && !pending.is_empty() {
        let item = pending.pop_front().unwrap();
        spawn_work_item(
            &mut join_set,
            item,
            worktree_mgr.clone(),
            repo_slug.to_string(),
            model.to_string(),
        );
    }

    // Process completions and fill new slots.
    while let Some(result) = join_set.join_next().await {
        let outcome = result.unwrap_or_else(|e| WorkItemOutcome {
            item_name: "unknown".to_string(),
            result: Err(anyhow::anyhow!("task panicked: {e}")),
            cost_usd: 0.0,
            duration: Duration::ZERO,
        });
        outcomes.push(outcome);

        // Fill the freed slot.
        if let Some(item) = pending.pop_front() {
            spawn_work_item(
                &mut join_set,
                item,
                worktree_mgr.clone(),
                repo_slug.to_string(),
                model.to_string(),
            );
        }
    }

    outcomes
}

fn spawn_work_item(
    join_set: &mut JoinSet<WorkItemOutcome>,
    item: WorkItem,
    mgr: Arc<WorktreeManager>,
    repo_slug: String,
    model: String,
) {
    let item_name = item.name();
    let budget = item.budget_cap;

    join_set.spawn(async move {
        let start = Instant::now();
        info!(item = %item_name, "starting work item");

        let result = match item.kind {
            WorkItemKind::CiFix { pr } => {
                let wt_name = format!("ci-fix-{}", pr.number);
                match mgr.create_at_branch(&wt_name, &pr.branch).await {
                    Ok(wt) => {
                        let r = phases::ci_fix::run(&pr, &repo_slug, &wt, &model, budget).await;
                        mgr.remove(&wt).await.ok();
                        r.map(|o| (WorkItemResult::CiFix {
                            pr_number: o.pr_number,
                            fixed: o.fixed,
                        }, o.cost_usd))
                    }
                    Err(e) => Err(e),
                }
            }
            WorkItemKind::PrReview { pr, fix_threshold } => {
                let wt_name = format!("review-{}", pr.number);
                match mgr.create_at_branch(&wt_name, &pr.branch).await {
                    Ok(wt) => {
                        let r = phases::pr_review::run(
                            &pr, &repo_slug, &wt, &model, budget, fix_threshold,
                        )
                        .await;
                        mgr.remove(&wt).await.ok();
                        r.map(|o| (WorkItemResult::PrReview {
                            pr_number: o.pr_number,
                            score: o.score,
                            fixed: o.fixed,
                            commented: o.commented,
                        }, o.cost_usd))
                    }
                    Err(e) => Err(e),
                }
            }
            WorkItemKind::IssueInvestigation {
                issue,
                repo_info,
                arch_summary,
                stack_info,
            } => {
                let wt_name = format!("issue-{}", issue.number);
                match mgr.create_from_head(&wt_name).await {
                    Ok(wt) => {
                        let r = phases::issue_investigation::run(
                            &issue,
                            &wt,
                            &repo_slug,
                            &repo_info,
                            &arch_summary,
                            &stack_info,
                            &model,
                            budget,
                        )
                        .await;
                        mgr.remove(&wt).await.ok();
                        r.map(|o| (WorkItemResult::IssueInvestigation {
                            issue_number: o.issue_number,
                            fixed: o.fixed,
                            pr_url: o.pr_url,
                        }, o.cost_usd))
                    }
                    Err(e) => Err(e),
                }
            }
            WorkItemKind::Analysis {
                clone_path,
                repo_info,
                arch_summary,
                stack_info,
                open_prs,
                model: analysis_model,
                max_tasks,
                min_severity,
                improve_docs,
                dry_run,
                critic_threshold,
                doc_critic_threshold,
            } => {
                run_analysis_pipeline(
                    &clone_path,
                    &repo_info,
                    &arch_summary,
                    &stack_info,
                    &open_prs,
                    &analysis_model,
                    max_tasks,
                    &min_severity,
                    improve_docs,
                    dry_run,
                    critic_threshold,
                    doc_critic_threshold,
                    budget,
                    &repo_slug,
                )
                .await
                .map(|(r, c)| (r, c))
            }
        };

        let (cost, work_result) = match result {
            Ok((r, cost)) => (cost, Ok(r)),
            Err(e) => (0.0, Err(e)),
        };

        WorkItemOutcome {
            item_name,
            result: work_result,
            cost_usd: cost,
            duration: start.elapsed(),
        }
    });
}

// ---------------------------------------------------------------------------
// Analysis pipeline (extracted from old sequential orchestrator)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_analysis_pipeline(
    clone_path: &PathBuf,
    repo_info: &RepoInfo,
    arch_summary: &str,
    stack_info: &StackInfo,
    open_prs: &[OpenPr],
    model: &str,
    max_tasks: usize,
    min_severity: &crate::models::Severity,
    improve_docs: bool,
    dry_run: bool,
    critic_threshold: u32,
    doc_critic_threshold: u32,
    mut budget: f64,
    _repo_slug: &str,
) -> Result<(WorkItemResult, f64)> {
    let mut cost_total = 0.0;

    // ─── Analysis ──────────────────────────────────────────────────────
    info!("starting analysis phase");
    let analysis_budget = (budget * 0.20).max(0.50).min(budget);

    let analysis_output = tokio::time::timeout(
        Duration::from_secs(600),
        phases::analysis::run(
            clone_path,
            arch_summary,
            stack_info,
            open_prs,
            model,
            analysis_budget,
            max_tasks,
            min_severity,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("analysis timed out"))?
    .context("analysis failed")?;

    budget -= analysis_output.cost_usd;
    cost_total += analysis_output.cost_usd;

    let improvements = analysis_output.improvements;
    let is_doc_improvements;

    // Doc fallback.
    let improvements = if improvements.is_empty() && improve_docs {
        info!("no code improvements found, falling back to documentation analysis");
        let doc_budget = (budget * 0.20).max(0.50).min(budget);
        let doc_output = tokio::time::timeout(
            Duration::from_secs(600),
            phases::analysis::run_doc_analysis(
                clone_path,
                arch_summary,
                stack_info,
                model,
                doc_budget,
                max_tasks,
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("doc analysis timed out"))?
        .context("doc analysis failed")?;

        budget -= doc_output.cost_usd;
        cost_total += doc_output.cost_usd;

        if doc_output.improvements.is_empty() {
            info!("no documentation improvements found either");
            return Ok((
                WorkItemResult::AnalysisPipeline {
                    pr_url: None,
                    branch_name: None,
                    pr_number: None,
                    has_successful_tasks: false,
                },
                cost_total,
            ));
        }
        is_doc_improvements = true;
        doc_output.improvements
    } else if improvements.is_empty() {
        info!("no actionable improvements found");
        return Ok((
            WorkItemResult::AnalysisPipeline {
                pr_url: None,
                branch_name: None,
                pr_number: None,
                has_successful_tasks: false,
            },
            cost_total,
        ));
    } else {
        is_doc_improvements = false;
        improvements
    };

    // Dry-run: print JSON and return.
    if dry_run {
        let json = serde_json::to_string_pretty(&improvements)
            .context("failed to serialize improvements to JSON")?;
        println!("{json}");
        return Ok((
            WorkItemResult::AnalysisPipeline {
                pr_url: None,
                branch_name: None,
                pr_number: None,
                has_successful_tasks: false,
            },
            cost_total,
        ));
    }

    // ─── Branch Creation ───────────────────────────────────────────────
    let branch_output = tokio::time::timeout(
        Duration::from_secs(60),
        phases::plan::create_branch(clone_path, &improvements),
    )
    .await
    .map_err(|_| anyhow::anyhow!("branch creation timed out"))?
    .context("branch creation failed")?;

    let branch_name = branch_output.branch_name;

    // ─── Implement ─────────────────────────────────────────────────────
    let implement_budget = budget * 0.60;
    let implement_output = tokio::time::timeout(
        Duration::from_secs(1800),
        phases::implement::run(
            clone_path,
            &improvements,
            stack_info,
            &branch_name,
            model,
            implement_budget,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("implement phase timed out"))?
    .context("implement phase failed")?;

    budget -= implement_output.total_cost_usd;
    cost_total += implement_output.total_cost_usd;

    let has_successful = implement_output
        .results
        .iter()
        .any(|r| matches!(r.status, TaskStatus::Success));

    if !has_successful {
        return Ok((
            WorkItemResult::AnalysisPipeline {
                pr_url: None,
                branch_name: Some(branch_name),
                pr_number: None,
                has_successful_tasks: false,
            },
            cost_total,
        ));
    }

    // ─── Critic Review ─────────────────────────────────────────────────
    let mut critic_summary: Option<String> = None;
    let threshold = if is_doc_improvements {
        doc_critic_threshold
    } else {
        critic_threshold
    };

    if threshold > 0 && budget > 0.0 {
        let critic_budget = budget.min(0.50);
        match tokio::time::timeout(
            Duration::from_secs(300),
            phases::critic::run(
                clone_path,
                &repo_info.default_branch,
                model,
                critic_budget,
            ),
        )
        .await
        {
            Ok(Ok(critic_output)) => {
                budget -= critic_output.cost_usd;
                cost_total += critic_output.cost_usd;

                if critic_output.score < threshold {
                    info!(
                        score = critic_output.score,
                        threshold,
                        "critic rejected changes"
                    );
                    // Critic rejected — mark as no successful tasks so
                    // cleanup deletes the lock branch from GitHub.
                    return Ok((
                        WorkItemResult::AnalysisPipeline {
                            pr_url: None,
                            branch_name: Some(branch_name),
                            pr_number: None,
                            has_successful_tasks: false,
                        },
                        cost_total,
                    ));
                }
                critic_summary = Some(format!(
                    "## Review\n\nScore: {}/10\n\n{}",
                    critic_output.score, critic_output.summary
                ));
            }
            Ok(Err(e)) => {
                warn!(error = %e, "critic review failed (non-fatal, proceeding)");
            }
            Err(_) => {
                warn!("critic review timed out (non-fatal, proceeding)");
            }
        }
    }

    // ─── Push (only after critic approves) ─────────────────────────────
    let push_output = tokio::process::Command::new("git")
        .args(["push", "-u", "origin", &branch_name, "--force-with-lease"])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to push")?;

    if !push_output.status.success() {
        let stderr = String::from_utf8_lossy(&push_output.stderr);
        anyhow::bail!("git push failed after critic approval: {stderr}");
    }
    info!(branch = %branch_name, "pushed changes to origin");

    // ─── PR Creation ───────────────────────────────────────────────────
    let plan_budget = budget.min(0.10);
    let pr_output = tokio::time::timeout(
        Duration::from_secs(120),
        phases::plan::create_pr(
            clone_path,
            repo_info,
            &branch_name,
            &improvements,
            model,
            plan_budget,
            critic_summary.as_deref(),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("PR creation timed out"))?
    .context("PR creation failed")?;

    cost_total += pr_output.cost_usd;

    Ok((
        WorkItemResult::AnalysisPipeline {
            pr_url: Some(pr_output.pr_url),
            branch_name: Some(branch_name),
            pr_number: Some(pr_output.pr_number),
            has_successful_tasks: true,
        },
        cost_total,
    ))
}
