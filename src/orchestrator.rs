use crate::cleanup::CleanupGuard;
use crate::config::Config;
use crate::logging;
use crate::models::{OpenPr, PhaseReport, TaskStatus};
use crate::phases;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

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
        phases::preflight::run(repo_slug),
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

    // ─── CI Fix Phase ──────────────────────────────────────────────────
    {
        let prs_to_fix = preflight_output.prs_needing_fix();
        if !prs_to_fix.is_empty() {
            info!(count = prs_to_fix.len(), "starting CI fix phase");
            for pr in prs_to_fix {
                if *budget_remaining <= 0.0 {
                    warn!("budget exhausted before CI fix");
                    break;
                }
                let phase_start = Instant::now();
                let fix_budget = budget_remaining.min(2.0);

                match tokio::time::timeout(
                    Duration::from_secs(600),
                    phases::ci_fix::run(pr, repo_slug, work_dir, &config.model, fix_budget),
                )
                .await
                {
                    Ok(Ok(output)) => {
                        *budget_remaining -= output.cost_usd;
                        *total_cost += output.cost_usd;
                        phases_report.push(PhaseReport {
                            name: format!("CI Fix (PR #{})", pr.number),
                            duration: phase_start.elapsed(),
                            cost_usd: output.cost_usd,
                            status: if output.fixed {
                                "OK".to_string()
                            } else {
                                "NO_CHANGES".to_string()
                            },
                        });
                    }
                    Ok(Err(e)) => {
                        warn!(pr_number = pr.number, error = %e, "CI fix failed (non-fatal)");
                        phases_report.push(PhaseReport {
                            name: format!("CI Fix (PR #{})", pr.number),
                            duration: phase_start.elapsed(),
                            cost_usd: 0.0,
                            status: format!("FAILED: {e}"),
                        });
                    }
                    Err(_) => {
                        warn!(pr_number = pr.number, "CI fix timed out");
                        phases_report.push(PhaseReport {
                            name: format!("CI Fix (PR #{})", pr.number),
                            duration: phase_start.elapsed(),
                            cost_usd: 0.0,
                            status: "TIMEOUT".to_string(),
                        });
                    }
                }
            }
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
    let recon_budget = budget_remaining.min(0.50);

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

    let clone_path = &recon_output.clone_path;
    let stack_info = &recon_output.stack_info;
    let arch_summary = &recon_output.arch_summary;

    // ─── Staleness check ──────────────────────────────────────────────
    // Skip analysis if no commits (on any branch) are newer than skip_after × cron_interval.
    if config.skip_after > 0 {
        let age_secs = phases::preflight::newest_commit_age_secs(clone_path).await;
        let threshold_secs = config.skip_after as u64 * config.cron_interval * 60;
        if age_secs > threshold_secs {
            info!(
                age_secs,
                threshold_secs,
                "no recent commits on any branch, skipping analysis"
            );
            println!(
                "No recent commits (newest is {}s old, threshold {}s). Skipping.",
                age_secs, threshold_secs
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

    // Merge recon open PRs with in-flight autoanneal PRs so analysis can avoid overlap.
    let mut open_prs: Vec<OpenPr> = recon_output.open_prs.clone();
    for ifp in &in_flight_prs {
        open_prs.push(OpenPr {
            number: ifp.number,
            title: ifp.title.clone(),
            head_ref: ifp.branch.clone(),
            files: vec![], // we don't know exact files; the body provides context
        });
    }
    let open_prs = open_prs;

    // ─── Phase 3: Analysis ──────────────────────────────────────────────
    if *budget_remaining <= 0.0 {
        warn!("budget exhausted before Analysis phase");
        return Ok(2);
    }

    info!("starting phase: Analysis");
    let phase_start = Instant::now();
    let analysis_budget = (*budget_remaining * 0.20).max(0.50).min(*budget_remaining);

    let analysis_output = match tokio::time::timeout(
        Duration::from_secs(600),
        phases::analysis::run(
            clone_path,
            arch_summary,
            stack_info,
            &open_prs,
            &config.model,
            analysis_budget,
            config.max_tasks,
            min_severity,
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            let cost = output.cost_usd;
            *budget_remaining -= cost;
            *total_cost += cost;

            let found_count = output.improvements.len();
            phases_report.push(PhaseReport {
                name: "Analysis".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: cost,
                status: format!("OK (found {} improvements)", found_count),
            });
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "analysis failed");
            phases_report.push(PhaseReport {
                name: "Analysis".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            return Ok(1);
        }
        Err(_) => {
            warn!("analysis timed out");
            phases_report.push(PhaseReport {
                name: "Analysis".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    let improvements = &analysis_output.improvements;

    // If no improvements found, exit cleanly.
    if improvements.is_empty() {
        info!("no actionable improvements found");
        println!("No actionable improvements found.");
        return Ok(0);
    }

    // If dry-run, print improvements as JSON and exit.
    if config.dry_run {
        let json = serde_json::to_string_pretty(improvements)
            .context("failed to serialize improvements to JSON")?;
        println!("{json}");
        return Ok(0);
    }

    // ─── Phase 4: Branch Creation (lock) ──────────────────────────────────
    info!("starting phase: Branch Creation");
    let phase_start = Instant::now();

    let branch_output = match tokio::time::timeout(
        Duration::from_secs(60),
        phases::plan::create_branch(clone_path, improvements),
    )
    .await
    {
        Ok(Ok(output)) => {
            // Update cleanup guard with branch name early.
            cleanup_guard.branch_name = Some(output.branch_name.clone());

            phases_report.push(PhaseReport {
                name: "Branch Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "OK".to_string(),
            });
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "branch creation failed");
            phases_report.push(PhaseReport {
                name: "Branch Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            return Ok(1);
        }
        Err(_) => {
            warn!("branch creation timed out");
            phases_report.push(PhaseReport {
                name: "Branch Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    let branch_name = &branch_output.branch_name;

    // ─── Phase 5: Implement ─────────────────────────────────────────────
    if *budget_remaining <= 0.0 {
        warn!("budget exhausted before Implement phase");
        return Ok(2);
    }

    info!("starting phase: Implement");
    let phase_start = Instant::now();
    let implement_budget = *budget_remaining * 0.60;

    let implement_output = match tokio::time::timeout(
        Duration::from_secs(1800),
        phases::implement::run(
            clone_path,
            improvements,
            stack_info,
            branch_name,
            &config.model,
            implement_budget,
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            let cost = output.total_cost_usd;
            *budget_remaining -= cost;
            *total_cost += cost;

            let successful = output
                .results
                .iter()
                .filter(|r| matches!(r.status, TaskStatus::Success))
                .count();
            let total_tasks = output.results.len();

            cleanup_guard.has_successful_tasks = successful > 0;

            phases_report.push(PhaseReport {
                name: "Implement".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: cost,
                status: format!("OK ({}/{} tasks)", successful, total_tasks),
            });

            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "implement phase failed");
            phases_report.push(PhaseReport {
                name: "Implement".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            return Ok(1);
        }
        Err(_) => {
            warn!("implement phase timed out");
            phases_report.push(PhaseReport {
                name: "Implement".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    // Check if any tasks succeeded.
    let has_successful = implement_output
        .results
        .iter()
        .any(|r| matches!(r.status, TaskStatus::Success));

    if !has_successful {
        error!("no implementation tasks succeeded");
        // Cleanup guard will delete the branch on drop (no PR, no successful tasks).
        return Ok(1);
    }

    // ─── Phase 6: PR Creation ───────────────────────────────────────────
    if *budget_remaining <= 0.0 {
        warn!("budget exhausted before PR Creation phase");
        return Ok(2);
    }

    info!("starting phase: PR Creation");
    let phase_start = Instant::now();
    let plan_budget = budget_remaining.min(0.10);

    let pr_output = match tokio::time::timeout(
        Duration::from_secs(120),
        phases::plan::create_pr(
            clone_path,
            &repo_info,
            branch_name,
            improvements,
            &config.model,
            plan_budget,
        ),
    )
    .await
    {
        Ok(Ok(output)) => {
            let cost = output.cost_usd;
            *budget_remaining -= cost;
            *total_cost += cost;

            // Update cleanup guard with PR number.
            cleanup_guard.pr_number = Some(output.pr_number);

            phases_report.push(PhaseReport {
                name: "PR Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: cost,
                status: "OK".to_string(),
            });
            output
        }
        Ok(Err(e)) => {
            error!(error = %e, "PR creation failed");
            phases_report.push(PhaseReport {
                name: "PR Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: format!("FAILED: {e}"),
            });
            // Implementation succeeded but PR creation failed.
            // Branch with commits still exists; leave it for manual recovery.
            return Ok(1);
        }
        Err(_) => {
            warn!("PR creation timed out");
            phases_report.push(PhaseReport {
                name: "PR Creation".to_string(),
                duration: phase_start.elapsed(),
                cost_usd: 0.0,
                status: "TIMEOUT".to_string(),
            });
            return Ok(2);
        }
    };

    // ─── Summary ────────────────────────────────────────────────────────
    println!("PR: {}", pr_output.pr_url);

    // Disarm cleanup guard — we completed successfully.
    cleanup_guard.disarm();

    Ok(0)
}

