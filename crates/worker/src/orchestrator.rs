use autoanneal_lib::cleanup::CleanupGuard;
use autoanneal_lib::config::Config;
use autoanneal_lib::logging;
use autoanneal_lib::models::{
    CiStatus, ExternalPr, GithubIssue, InFlightPr, OpenPr, PhaseReport, RepoInfo, StackInfo,
    TaskStatus,
};
use autoanneal_lib::llm::{self, LlmInvocation};
use autoanneal_lib::phases;
use autoanneal_lib::prompts::system::critic_fix_system_prompt;
use autoanneal_lib::worktree::WorktreeManager;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

/// Global counter used as a fallback when the system clock is unreliable
/// (e.g. returns a time before `UNIX_EPOCH`). Each invocation produces a
/// unique, monotonically-increasing value so that work directory names never
/// collide.
/// 
/// Uses `Ordering::Relaxed` because:
/// - The counter is only used for uniqueness, not synchronization with other data
/// - We only need atomic increments to avoid duplicate values during races
/// - No happens-before relationship is required with other memory operations
static TIMESTAMP_FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Return a monotonically-increasing timestamp suitable for use in directory
/// names.
///
/// Uses the wall-clock time (seconds since UNIX epoch) when available. If the
/// system clock is behind the epoch — which causes
/// `duration_since(UNIX_EPOCH)` to fail — the function falls back to an
/// atomic counter that is guaranteed to produce a unique value on every call,
/// preventing silent directory-name collisions.
fn unique_timestamp_secs() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => TIMESTAMP_FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Work-queue types
// ---------------------------------------------------------------------------

/// A single unit of concurrent work.
struct WorkItem {
    kind: WorkItemKind,
    context_window: u64,
    exa_searches: u32,
}

enum WorkItemKind {
    CiFix {
        pr: InFlightPr,
        default_branch: String,
    },
    PrReview {
        pr: ExternalPr,
        fix_threshold: u32,
        default_branch: String,
        critic_models: Option<Vec<String>>,
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
        model_analysis: String,
        model_implement: String,
        model_critic: String,
        model_plan: String,
        max_tasks: usize,
        min_severity: autoanneal_lib::models::Severity,
        improve_docs: bool,
        dry_run: bool,
        critic_threshold: u32,
        doc_critic_threshold: u32,
        critic_models: Option<Vec<String>>,
    },
}

impl WorkItem {
    fn name(&self) -> String {
        match &self.kind {
            WorkItemKind::CiFix { pr, .. } => format!("CI Fix (PR #{})", pr.number),
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
    /// Work item was skipped before execution.
    Skipped {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Output of a worker run, including exit code and collected result data.
/// Internal output from run_pipeline.
struct PipelineOutput {
    exit_code: i32,
    work_item_summaries: Vec<WorkItemSummary>,
    pr_url: Option<String>,
}

impl PipelineOutput {
    fn early_exit(exit_code: i32) -> Self {
        Self {
            exit_code,
            work_item_summaries: vec![],
            pr_url: None,
        }
    }
}

pub struct RunOutput {
    pub exit_code: i32,
    pub repo_slug: String,
    pub total_cost: f64,
    pub phases: Vec<PhaseReport>,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    pub branch_name: Option<String>,
    pub work_items: Vec<WorkItemSummary>,
}

/// Summary of a work item for result reporting.
pub struct WorkItemSummary {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub cost_usd: f64,
    pub duration_secs: u64,
    pub pr_url: Option<String>,
}

/// Run the full autoworker pipeline. Returns a RunOutput with exit code and metrics.
pub async fn run(config: &Config) -> Result<RunOutput> {
    // --- Setup ---
    let repo_slug = config.repo_slug();
    let timeout_duration = config.timeout_duration();
    let min_severity = config.min_severity();
    let mut phases_report: Vec<PhaseReport> = Vec::new();
    let mut total_cost: f64 = 0.0;

    // Create work directory.
    let timestamp = unique_timestamp_secs();
    let work_dir = PathBuf::from(format!("/tmp/autoanneal-{timestamp}-{}", std::process::id()));
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
            &mut phases_report,
            &mut total_cost,
            &mut cleanup_guard,
        )
        .await
    })
    .await;

    match result {
        Ok(outcome) => {
            let pipeline_out = outcome?;

            // Print summary on all paths.
            logging::print_summary(
                &repo_slug,
                cleanup_guard.branch_name.as_deref(),
                None, // PR URL printed separately below
                &phases_report,
                total_cost,
            );

            Ok(RunOutput {
                exit_code: pipeline_out.exit_code,
                repo_slug,
                total_cost,
                phases: phases_report,
                pr_url: pipeline_out.pr_url,
                pr_number: cleanup_guard.pr_number,
                branch_name: cleanup_guard.branch_name.clone(),
                work_items: pipeline_out.work_item_summaries,
            })
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
            Ok(RunOutput {
                exit_code: 2,
                repo_slug,
                total_cost,
                phases: phases_report,
                pr_url: None,
                pr_number: cleanup_guard.pr_number,
                branch_name: cleanup_guard.branch_name.clone(),
                work_items: vec![],
            })
        }
    }
}

/// Inner pipeline, separated so it can be wrapped in a global timeout.
async fn run_pipeline(
    config: &Config,
    repo_slug: &str,
    min_severity: &autoanneal_lib::models::Severity,
    work_dir: &PathBuf,
    phases_report: &mut Vec<PhaseReport>,
    total_cost: &mut f64,
    cleanup_guard: &mut CleanupGuard,
) -> Result<PipelineOutput> {
    // ─── Phase 1: Preflight ─────────────────────────────────────────────
    info!("starting phase: Preflight");
    let phase_start = Instant::now();

    let preflight_output = match tokio::time::timeout(
        Duration::from_secs(60),
        phases::preflight::run(
            repo_slug,
            config.review_prs,
            config.force_review,
            &config.review_filter,
            &config.investigate_issues,
            config.fix_external_ci,
            config.fix_conflicts,
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
            return Ok(PipelineOutput { exit_code: 1, work_item_summaries: vec![], pr_url: None });
        }
        Err(_) => {
            warn!("preflight timed out");
            phases_report.push(PhaseReport {
                name: "Preflight".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(PipelineOutput { exit_code: 2, work_item_summaries: vec![], pr_url: None });
        }
    };

    // ─── Early exit checks (before recon to save money) ────────────────
    let has_external_ci_failures = config.fix_external_ci
        && preflight_output.external_prs.iter().any(|pr| {
            pr.ci_status == CiStatus::Failing
                && pr.autoanneal_commit_count < config.max_pr_fix_attempts as u64
        });
    let has_external_conflicts = config.fix_conflicts
        && preflight_output.external_prs.iter().any(|pr| pr.has_merge_conflicts);
    let has_maintenance = !preflight_output.prs_needing_ci_fix().is_empty()
        || !preflight_output.prs_needing_rebase().is_empty()
        || has_external_ci_failures
        || has_external_conflicts;
    let has_reviews = preflight_output.external_prs.iter().any(|pr| config.force_review || !pr.reviewed);
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
        return Ok(PipelineOutput::early_exit(0));
    }

    let has_work = has_maintenance || has_reviews || has_issues;

    if config.skip_after > 0 && !has_work {
        let threshold_secs = (config.skip_after as u64)
            .saturating_mul(config.cron_interval)
            .saturating_mul(60);
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
            return Ok(PipelineOutput::early_exit(0));
        }
    }

    let repo_info = preflight_output.repo_info;
    let in_flight_prs = preflight_output.in_flight_prs;

    // ─── Phase 2: Recon ─────────────────────────────────────────────────
    info!("starting phase: Recon");
    let phase_start = Instant::now();

    let recon_output = match tokio::time::timeout(
        Duration::from_secs(300),
        phases::recon::run(
            &repo_info,
            work_dir,
            config.model_for("recon"),
            config.setup_command.as_deref(),
            config.context_window,
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            let cost = output.cost_usd;
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
            return Ok(PipelineOutput::early_exit(1));
        }
        Err(_) => {
            warn!("recon timed out");
            phases_report.push(PhaseReport {
                name: "Recon".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(PipelineOutput::early_exit(2));
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
        config.context_window,
    );

    if work_items.is_empty() {
        info!("no work items to process");
        println!("No actionable work items found.");
        return Ok(PipelineOutput::early_exit(0));
    }

    info!(count = work_items.len(), "built work queue");

    // ─── Execute work queue ─────────────────────────────────────────────
    // CI fix items run sequentially (concurrency=1) because they may spawn
    // heavy child processes (cargo check, etc.) that compete for memory.
    // All other items run at the configured concurrency level.
    let (ci_fix_items, other_items): (Vec<_>, Vec<_>) = work_items
        .into_iter()
        .partition(|item| matches!(item.kind, WorkItemKind::CiFix { .. }));

    let mut all_outcomes = Vec::new();

    if !ci_fix_items.is_empty() {
        info!(count = ci_fix_items.len(), "running CI fix items sequentially");
        let ci_outcomes = run_work_queue(
            1, // sequential
            ci_fix_items,
            worktree_mgr.clone(),
            repo_slug,
            &config.model,
        )
        .await;
        all_outcomes.extend(ci_outcomes);
    }

    if !other_items.is_empty() {
        let other_outcomes = run_work_queue(
            config.concurrency,
            other_items,
            worktree_mgr,
            repo_slug,
            &config.model,
        )
        .await;
        all_outcomes.extend(other_outcomes);
    }

    let outcomes = all_outcomes;

    // ─── Process outcomes ────────────────────────────────────────────────
    let exit_code = 0;
    let mut work_item_summaries: Vec<WorkItemSummary> = Vec::new();
    let mut best_pr_url: Option<String> = None;

    for outcome in &outcomes {
        *total_cost += outcome.cost_usd;

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
                        if best_pr_url.is_none() {
                            best_pr_url = Some(url.clone());
                        }
                        "OK".to_string()
                    } else if *has_successful_tasks {
                        // Critic rejected — this is a normal outcome, not an error.
                        cleanup_guard.branch_name = branch_name.clone();
                        cleanup_guard.has_successful_tasks = false; // delete the branch
                        "REJECTED (critic)".to_string()
                    } else {
                        // Analysis found nothing or all tasks failed — also normal.
                        cleanup_guard.branch_name = branch_name.clone();
                        "NO_IMPROVEMENTS".to_string()
                    }
                }
                WorkItemResult::Skipped { reason } => format!("SKIPPED ({reason})"),
            },
            Err(e) => {
                warn!(item = %outcome.item_name, error = %e, "work item failed");
                format!("FAILED: {e}")
            }
        };

        // Collect work item summary for result reporting.
        let item_pr_url = match &outcome.result {
            Ok(WorkItemResult::AnalysisPipeline { pr_url: Some(u), .. })
            | Ok(WorkItemResult::IssueInvestigation { pr_url: Some(u), .. }) => Some(u.clone()),
            _ => None,
        };
        work_item_summaries.push(WorkItemSummary {
            kind: classify_work_item(&outcome.item_name),
            name: outcome.item_name.clone(),
            status: status.clone(),
            cost_usd: outcome.cost_usd,
            duration_secs: outcome.duration.as_secs(),
            pr_url: item_pr_url,
        });

        phases_report.push(PhaseReport {
            name: outcome.item_name.clone(),
            duration: outcome.duration,
            cost_usd: outcome.cost_usd,
            status,
        });
    }

    Ok(PipelineOutput {
        exit_code,
        work_item_summaries: work_item_summaries,
        pr_url: best_pr_url,
    })
}

/// Classify a work item name into a kind string.
fn classify_work_item(name: &str) -> String {
    if name.starts_with("CI Fix") {
        "ci_fix".to_string()
    } else if name.starts_with("PR Review") {
        "pr_review".to_string()
    } else if name.starts_with("Issue #") {
        "issue_investigation".to_string()
    } else if name == "Analysis Pipeline" {
        "analysis".to_string()
    } else {
        "unknown".to_string()
    }
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
    min_severity: &autoanneal_lib::models::Severity,
    context_window: u64,
) -> Vec<WorkItem> {
    let mut items = Vec::new();

    // External PR CI fix and merge conflict items.
    if config.fix_external_ci || config.fix_conflicts {
        for ext_pr in external_prs.iter() {
            let needs_ci_fix = config.fix_external_ci && ext_pr.ci_status == CiStatus::Failing;
            let needs_conflict_fix = config.fix_conflicts && ext_pr.has_merge_conflicts;
            if !needs_ci_fix && !needs_conflict_fix {
                continue;
            }
            // Skip PRs that have hit the CI fix attempt limit.
            if needs_ci_fix && ext_pr.autoanneal_commit_count >= config.max_pr_fix_attempts as u64 {
                info!(
                    pr_number = ext_pr.number,
                    attempts = ext_pr.autoanneal_commit_count,
                    max = config.max_pr_fix_attempts,
                    "skipping external CI fix — attempt limit reached"
                );
                continue;
            }
            // Convert ExternalPr to InFlightPr for the CI fix phase.
            let as_inflight = InFlightPr {
                number: ext_pr.number,
                title: ext_pr.title.clone(),
                body: String::new(),
                branch: ext_pr.branch.clone(),
                ci_status: ext_pr.ci_status,
                has_fixing_label: false,
                has_merge_conflicts: ext_pr.has_merge_conflicts,
                files: Vec::new(),
            };
            items.push(WorkItem {
                kind: WorkItemKind::CiFix {
                    pr: as_inflight,
                    default_branch: repo_info.default_branch.clone(),
                },
                context_window,
                exa_searches: config.exa_searches,
            });
        }
    }

    // CI fix items (autoanneal PRs).
    if config.fix_ci || config.fix_conflicts {
        let mut prs_to_fix: Vec<&InFlightPr> = Vec::new();
        if config.fix_ci {
            prs_to_fix.extend(
                in_flight_prs
                    .iter()
                    .filter(|pr| {
                        pr.ci_status == autoanneal_lib::models::CiStatus::Failing && !pr.has_fixing_label
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
            items.push(WorkItem {
                kind: WorkItemKind::CiFix {
                    pr: pr.clone(),
                    default_branch: repo_info.default_branch.clone(),
                },
                context_window,
                exa_searches: config.exa_searches,
            });
        }
    }

    // PR review items — skip PRs already reviewed unless force_review is set.
    if config.review_prs || config.force_review {
        for pr in external_prs.iter().filter(|pr| config.force_review || !pr.reviewed).take(3) {
            items.push(WorkItem {
                kind: WorkItemKind::PrReview {
                    pr: pr.clone(),
                    fix_threshold: config.review_fix_threshold,
                    default_branch: repo_info.default_branch.clone(),
                    critic_models: config.critic_model_list(),
                },
                context_window,
                exa_searches: config.exa_searches,
            });
        }
        // When force_review is set, also review in-flight (autoanneal/) PRs.
        if config.force_review {
            for ifp in in_flight_prs.iter().take(5) {
                let as_external = ExternalPr {
                    number: ifp.number,
                    title: ifp.title.clone(),
                    branch: ifp.branch.clone(),
                    author: String::new(),
                    updated_at: String::new(),
                    labels: vec![],
                    ci_status: ifp.ci_status.clone(),
                    reviewed: false,
                    autoanneal_commit_count: 0,
                    has_merge_conflicts: ifp.has_merge_conflicts,
                };
                items.push(WorkItem {
                    kind: WorkItemKind::PrReview {
                        pr: as_external,
                        fix_threshold: config.review_fix_threshold,
                        default_branch: repo_info.default_branch.clone(),
                        critic_models: config.critic_model_list(),
                    },
                    context_window,
                    exa_searches: config.exa_searches,
                });
            }
        }
    }

    // Issue investigation items.
    for issue in issues.iter().take(config.max_issues) {
        items.push(WorkItem {
            kind: WorkItemKind::IssueInvestigation {
                issue: issue.clone(),
                repo_info: repo_info.clone(),
                arch_summary: arch_summary.to_string(),
                stack_info: stack_info.clone(),
            },
            context_window,
            exa_searches: config.exa_searches,
        });
    }

    // Merge open PRs with in-flight autoanneal PRs for analysis overlap avoidance.
    // Deduplicate: an in-flight autoanneal PR may already appear in open_prs.
    let mut merged_open_prs: Vec<OpenPr> = open_prs.to_vec();
    for ifp in in_flight_prs {
        if merged_open_prs.iter().any(|p| p.number == ifp.number) {
            continue;
        }
        // Extract file paths from the PR's files field.
        let files = ifp.files.clone();
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
    // Run analysis when either we're not skipping (normal mode) or we're in
    // dry-run mode (dry-run doesn't create PRs, so the open-PR cap doesn't apply).
    if !skip_analysis || config.dry_run {
        items.push(WorkItem {
            kind: WorkItemKind::Analysis {
                clone_path: clone_path.clone(),
                repo_info: repo_info.clone(),
                arch_summary: arch_summary.to_string(),
                stack_info: stack_info.clone(),
                open_prs: merged_open_prs,
                model_analysis: config.model_for("analysis").to_string(),
                model_implement: config.model_for("implement").to_string(),
                model_critic: config.model_for("critic").to_string(),
                model_plan: config.model_for("plan").to_string(),
                max_tasks: config.max_tasks,
                min_severity: *min_severity,
                improve_docs: config.improve_docs,
                dry_run: config.dry_run,
                critic_threshold: config.critic_threshold,
                doc_critic_threshold: config.doc_critic_threshold,
                critic_models: config.critic_model_list(),
            },
            context_window,
            exa_searches: config.exa_searches,
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
    while join_set.len() < concurrency {
        let item = match pending.pop_front() {
            Some(item) => item,
            None => break,
        };
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
    let context_window = item.context_window;
    let exa_searches = item.exa_searches;

    join_set.spawn(async move {
        let start = Instant::now();
        info!(item = %item_name, "starting work item");

        let result = match item.kind {
            WorkItemKind::CiFix { pr, default_branch } => {
                let wt_name = format!("ci-fix-{}", pr.number);
                match mgr.create_at_branch(&wt_name, &pr.branch).await {
                    Ok(wt) => {
                        let r = phases::ci_fix::run(&pr, &repo_slug, &wt, &model, &default_branch, context_window).await;
                        if let Err(e) = mgr.remove(&wt).await {
                            warn!(worktree = %wt_name, error = %e, "failed to clean up worktree");
                        }
                        r.map(|o| (WorkItemResult::CiFix {
                            pr_number: o.pr_number,
                            fixed: o.fixed,
                        }, o.cost_usd))
                    }
                    Err(e) => Err(e),
                }
            }
            WorkItemKind::PrReview { pr, fix_threshold, default_branch, critic_models } => {
                let wt_name = format!("review-{}", pr.number);
                match mgr.create_at_branch(&wt_name, &pr.branch).await {
                    Ok(wt) => {
                        let r = phases::pr_review::run(
                            &pr, &repo_slug, &wt, &model, fix_threshold, context_window,
                            critic_models.as_deref(), &default_branch,
                        )
                        .await;
                        if let Err(e) = mgr.remove(&wt).await {
                            warn!(worktree = %wt_name, error = %e, "failed to clean up worktree");
                        }
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
                            context_window,
                        )
                        .await;
                        if let Err(e) = mgr.remove(&wt).await {
                            warn!(worktree = %wt_name, error = %e, "failed to clean up worktree");
                        }
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
                model_analysis,
                model_implement,
                model_critic,
                model_plan,
                max_tasks,
                min_severity,
                improve_docs,
                dry_run,
                critic_threshold,
                doc_critic_threshold,
                critic_models,
            } => {
                run_analysis_pipeline(
                    &clone_path,
                    &repo_info,
                    &arch_summary,
                    &stack_info,
                    &open_prs,
                    &model_analysis,
                    &model_implement,
                    &model_critic,
                    &model_plan,
                    max_tasks,
                    &min_severity,
                    improve_docs,
                    dry_run,
                    critic_threshold,
                    doc_critic_threshold,
                    &critic_models,
                    &repo_slug,
                    context_window,
                    exa_searches,
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
    model_analysis: &str,
    model_implement: &str,
    model_critic: &str,
    model_plan: &str,
    max_tasks: usize,
    min_severity: &autoanneal_lib::models::Severity,
    improve_docs: bool,
    dry_run: bool,
    critic_threshold: u32,
    doc_critic_threshold: u32,
    critic_models: &Option<Vec<String>>,
    _repo_slug: &str,
    context_window: u64,
    exa_searches: u32,
) -> Result<(WorkItemResult, f64)> {
    let mut cost_total = 0.0;

    // ─── Analysis ──────────────────────────────────────────────────────
    info!("starting analysis phase");

    let analysis_output = tokio::time::timeout(
        Duration::from_secs(900),
        phases::analysis::run(
            clone_path,
            arch_summary,
            stack_info,
            open_prs,
            model_analysis,
            max_tasks,
            min_severity,
            context_window,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("analysis timed out"))?
    .context("analysis failed")?;

    cost_total += analysis_output.cost_usd;

    let improvements = analysis_output.improvements;
    let is_doc_improvements;

    // Doc fallback.
    let improvements = if improvements.is_empty() && improve_docs {
        info!("no code improvements found, falling back to documentation analysis");
        let doc_output = tokio::time::timeout(
            Duration::from_secs(900),
            phases::analysis::run_doc_analysis(
                clone_path,
                arch_summary,
                stack_info,
                model_analysis,
                max_tasks,
                min_severity,
                context_window,
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("doc analysis timed out"))?
        .context("doc analysis failed")?;

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

    // ─── File-overlap dedup against in-flight PRs ──────────────────────
    // Remove improvements whose files are already being modified by an
    // open autoanneal PR, even if the LLM analysis didn't notice.
    let in_flight_files: std::collections::HashSet<&str> = open_prs
        .iter()
        .flat_map(|pr| pr.files.iter().map(|f| f.as_str()))
        .collect();

    let before_dedup = improvements.len();
    let improvements: Vec<_> = improvements
        .into_iter()
        .filter(|imp| {
            let dominated = imp
                .files_to_modify
                .iter()
                .any(|f| in_flight_files.contains(f.as_str()));
            if dominated {
                info!(
                    title = %imp.title,
                    "skipping improvement: files overlap with in-flight PR"
                );
            }
            !dominated
        })
        .collect();

    if improvements.len() < before_dedup {
        info!(
            before = before_dedup,
            after = improvements.len(),
            "dedup: removed improvements overlapping with in-flight PRs"
        );
    }

    if improvements.is_empty() {
        info!("all improvements overlap with in-flight PRs, nothing to do");
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
    let implement_output = tokio::time::timeout(
        Duration::from_secs(1800),
        phases::implement::run(
            clone_path,
            &improvements,
            stack_info,
            &branch_name,
            model_implement,
            context_window,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("implement phase timed out"))?
    .context("implement phase failed")?;

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
    // (made_fixes, initial_score, initial_summary, final_score, score_unverified)
    let mut critic_fix_info: Option<(bool, Option<u32>, Option<String>, u32, bool)> = None;
    let threshold = if is_doc_improvements {
        doc_critic_threshold
    } else {
        critic_threshold
    };

    if threshold > 0 {
        let critic_result = if let Some(models) = critic_models {
            // Panel mode: 2-gate deliberation
            info!("using critic panel with {} model(s)", models.len());
            let initial_panel = match tokio::time::timeout(
                Duration::from_secs(900),
                phases::critic_panel::run(
                    clone_path,
                    &repo_info.default_branch,
                    models,
                    context_window,
                    false, // skip_gate1
                    exa_searches,
                ),
            )
            .await
            {
                Ok(Ok(result)) => Some(result),
                Ok(Err(e)) => {
                    warn!(error = %e, "critic panel failed (non-fatal, proceeding)");
                    None
                }
                Err(_) => {
                    warn!("critic panel timed out (non-fatal, proceeding)");
                    None
                }
            };

            // ─── Panel fix loop ─────────────────────────────────────
            // If the panel returned needs_work, attempt up to 2 fix rounds
            // using a single critic model, then re-run the panel.
            const MAX_FIX_ROUNDS: u32 = 2;
            let mut panel_result = initial_panel;
            if let Some(ref mut cr) = panel_result {
                // Capture initial state before fix loop mutates cr.
                let pre_fix_summary = cr.summary.clone();
                let pre_fix_score = cr.score;
                for fix_round in 1..=MAX_FIX_ROUNDS {
                    if cr.verdict != "needs_work" {
                        break;
                    }

                    info!(
                        fix_round,
                        score = cr.score,
                        "panel fix loop: attempting fix"
                    );
                    let fix_prompt = format!(
                        "A code review panel found issues with the implementation.\n\n\
                         ## Review Summary\n\n{summary}\n\n\
                         ## Instructions\n\n\
                         Fix the issues identified above. Make minimal, focused changes.",
                        summary = cr.summary
                    );

                    let fix_invocation = LlmInvocation {
                        prompt: fix_prompt,
                        system_prompt: Some(critic_fix_system_prompt()),
                        model: model_critic.to_string(),
                        max_turns: 50,
                        effort: "high",
                        tools: "Read,Glob,Grep,Bash,Edit,Write",
                        json_schema: None,
                        working_dir: clone_path.to_path_buf(),
                        context_window,
                        provider_hint: None,
                        max_tokens_per_turn: None,
                        ci_context: None,
                        exa_max_searches: 0,
                    };

                    let fix_response = llm::invoke::<serde_json::Value>(
                        &fix_invocation,
                        Duration::from_secs(600),
                    )
                    .await;

                    match fix_response {
                        Ok(resp) => {
                            cr.cost_usd += resp.cost_usd;

                            // Check for changes
                            let has_changes = tokio::process::Command::new("git")
                                .args(["diff", "--stat"])
                                .current_dir(clone_path)
                                .output()
                                .await
                                .map(|o| {
                                    !String::from_utf8_lossy(&o.stdout).trim().is_empty()
                                })
                                .unwrap_or(false);

                            if !has_changes {
                                info!(fix_round, "panel fix: no changes produced, stopping");
                                break;
                            }

                            // Stage and commit
                            let add_ok = tokio::process::Command::new("git")
                                .args(["add", "-A"])
                                .current_dir(clone_path)
                                .output()
                                .await
                                .map(|o| o.status.success())
                                .unwrap_or(false);

                            let commit_msg = format!(
                                "autoanneal: address review feedback (round {})",
                                fix_round
                            );
                            let commit_ok = add_ok
                                && tokio::process::Command::new("git")
                                    .args(["commit", "-m", &commit_msg])
                                    .current_dir(clone_path)
                                    .output()
                                    .await
                                    .map(|o| o.status.success())
                                    .unwrap_or(false);

                            if !commit_ok {
                                warn!(fix_round, "panel fix: git commit failed, stopping");
                                break;
                            }

                            info!(fix_round, "panel fix: committed changes, re-reviewing");

                            // Re-run panel with skip_gate1=true
                            match tokio::time::timeout(
                                Duration::from_secs(900),
                                phases::critic_panel::run(
                                    clone_path,
                                    &repo_info.default_branch,
                                    models,
                                    context_window,
                                    true, // skip_gate1
                                    exa_searches,
                                ),
                            )
                            .await
                            {
                                Ok(Ok(re_result)) => {
                                    info!(
                                        fix_round,
                                        old_score = cr.score,
                                        new_score = re_result.score,
                                        new_verdict = %re_result.verdict,
                                        "panel fix: re-review complete"
                                    );
                                    // Update the result in place
                                    cr.score = re_result.score;
                                    cr.verdict = re_result.verdict;
                                    cr.summary = re_result.summary;
                                    cr.cost_usd += re_result.cost_usd;
                                    cr.made_fixes = true;
                                }
                                Ok(Err(e)) => {
                                    warn!(
                                        fix_round,
                                        error = %e,
                                        "panel fix: re-review failed, stopping"
                                    );
                                    cr.made_fixes = true;
                                    cr.score_unverified = true;
                                    break;
                                }
                                Err(_) => {
                                    warn!(
                                        fix_round,
                                        "panel fix: re-review timed out, stopping"
                                    );
                                    cr.made_fixes = true;
                                    cr.score_unverified = true;
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                fix_round,
                                error = %e,
                                "panel fix: fix agent failed, stopping"
                            );
                            break;
                        }
                    }
                }
                // If fixes were made, record the initial state for PR body formatting.
                if cr.made_fixes {
                    cr.initial_summary = Some(pre_fix_summary);
                    cr.initial_score = Some(pre_fix_score);
                }
            }
            panel_result
        } else {
            // Single critic mode (existing behavior)
            match tokio::time::timeout(
                Duration::from_secs(900),
                phases::critic::run(
                    clone_path,
                    &repo_info.default_branch,
                    model_critic,
                    context_window,
                ),
            )
            .await
            {
                Ok(Ok(result)) => Some(result),
                Ok(Err(e)) => {
                    warn!(error = %e, "critic review failed (non-fatal, proceeding)");
                    None
                }
                Err(_) => {
                    warn!("critic review timed out (non-fatal, proceeding)");
                    None
                }
            }
        };

        // Save fix metadata before consuming critic_result.
        critic_fix_info = critic_result.as_ref().map(|c| {
            (c.made_fixes, c.initial_score, c.initial_summary.clone(), c.score, c.score_unverified)
        });

        if let Some(critic_output) = critic_result {
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

            critic_summary = if critic_output.made_fixes {
                let initial_part = critic_output.initial_summary
                    .as_deref()
                    .unwrap_or("(initial review unavailable)");
                let initial_score = critic_output.initial_score.unwrap_or(0);
                // Strikethrough each line of the initial issues.
                let struck = initial_part.lines()
                    .map(|line| if line.trim().is_empty() { String::new() } else { format!("~~{}~~", line) })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!(
                    "## Review\n\n\
                     **Initial review: {initial_score}/10**\n\n\
                     {struck}\n\n\
                     **After fixes: {}/10**\n\n\
                     {}",
                    critic_output.score, critic_output.summary
                ))
            } else {
                Some(format!(
                    "## Review\n\nScore: {}/10\n\n{}",
                    critic_output.score, critic_output.summary
                ))
            };
        } else {
            critic_summary = Some("## Review\n\nCritic review unavailable.".to_string());
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
    let pr_output = tokio::time::timeout(
        Duration::from_secs(120),
        phases::plan::create_pr(
            clone_path,
            repo_info,
            &branch_name,
            &improvements,
            model_plan,
            critic_summary.as_deref(),
            context_window,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("PR creation timed out"))?
    .context("PR creation failed")?;

    cost_total += pr_output.cost_usd;

    // Post a comment if the critic made fixes, so reviewers see the progression.
    if let Some((true, initial_score, _, final_score, unverified)) = critic_fix_info {
        let score_note = if unverified {
            " (score unverified — re-review was skipped)"
        } else {
            ""
        };
        let comment_body = format!(
            "The automated reviewer found issues (initial score: {}/10) and applied fixes \
             during review, improving the score to {}/10{}.\n\n\
             See the PR description for the full review progression.",
            initial_score.unwrap_or(0),
            final_score,
            score_note,
        );
        let repo_slug = format!("{}/{}", repo_info.owner, repo_info.name);
        if let Err(e) = autoanneal_lib::retry::gh_command(
            clone_path,
            &[
                "pr", "comment",
                &pr_output.pr_number.to_string(),
                "--body", &comment_body,
                "-R", &repo_slug,
            ],
        ).await {
            warn!(error = %e, "failed to post critic-fix comment (non-fatal)");
        }
    }

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
