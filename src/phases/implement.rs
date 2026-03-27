use crate::claude::{self, ClaudeInvocation, ClaudeResponse};
use crate::guardrails;
use crate::models::{Category, Improvement, StackInfo, TaskResult, TaskStatus};
use crate::prompts::implement::IMPLEMENT_PROMPT;
use crate::prompts::system::implement_system_prompt;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{info, warn};

/// Maximum number of worktree groups running in parallel per batch.
const MAX_PARALLEL_GROUPS: usize = 5;

/// Timeout for a single Claude task invocation.
const TASK_TIMEOUT: Duration = Duration::from_secs(600);

pub struct ImplementOutput {
    pub results: Vec<TaskResult>,
    pub total_cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    clone_path: &Path,
    improvements: &[Improvement],
    stack_info: &StackInfo,
    branch_name: &str,
    model: &str,
    budget: f64,
) -> Result<ImplementOutput> {
    if improvements.is_empty() {
        return Ok(ImplementOutput {
            results: vec![],
            total_cost_usd: 0.0,
        });
    }

    let groups = partition_tasks(improvements);
    info!(
        num_groups = groups.len(),
        num_tasks = improvements.len(),
        "partitioned tasks into independent groups"
    );

    let mut all_results: Vec<TaskResult> = Vec::new();
    let mut total_cost_usd: f64 = 0.0;

    // Process groups in batches of up to MAX_PARALLEL_GROUPS.
    for batch_start in (0..groups.len()).step_by(MAX_PARALLEL_GROUPS) {
        let batch_end = (batch_start + MAX_PARALLEL_GROUPS).min(groups.len());
        let batch_groups = &groups[batch_start..batch_end];

        let remaining_budget = budget - total_cost_usd;
        let batch_output = run_batch(
            clone_path,
            improvements,
            batch_groups,
            stack_info,
            branch_name,
            model,
            remaining_budget,
        )
        .await?;

        total_cost_usd += batch_output.total_cost_usd;
        all_results.extend(batch_output.results);
    }

    Ok(ImplementOutput {
        results: all_results,
        total_cost_usd,
    })
}

// ---------------------------------------------------------------------------
// Task partitioning via union-find
// ---------------------------------------------------------------------------

/// Group task indices by file overlap. Tasks sharing any file end up in the
/// same group and must run sequentially; independent groups can run in parallel.
fn partition_tasks(improvements: &[Improvement]) -> Vec<Vec<usize>> {
    let n = improvements.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path compression
            x = parent[x];
        }
        x
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    // Map file -> first task index that touches it, then union subsequent tasks.
    let mut file_to_task: HashMap<&str, usize> = HashMap::new();
    for (idx, imp) in improvements.iter().enumerate() {
        for file in &imp.files_to_modify {
            if let Some(&prev_idx) = file_to_task.get(file.as_str()) {
                union(&mut parent, prev_idx, idx);
            } else {
                file_to_task.insert(file.as_str(), idx);
            }
        }
    }

    // Collect groups.
    let mut groups_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups_map.entry(root).or_default().push(i);
    }

    let mut groups: Vec<Vec<usize>> = groups_map.into_values().collect();
    // Sort each group by original index so tasks run in the intended order.
    for g in &mut groups {
        g.sort_unstable();
    }
    // Sort groups by their first task index for deterministic ordering.
    groups.sort_by_key(|g| g[0]);
    groups
}

// ---------------------------------------------------------------------------
// Batch execution (one batch = up to MAX_PARALLEL_GROUPS concurrent groups)
// ---------------------------------------------------------------------------

struct BatchOutput {
    results: Vec<TaskResult>,
    total_cost_usd: f64,
}

async fn run_batch(
    clone_path: &Path,
    improvements: &[Improvement],
    batch_groups: &[Vec<usize>],
    stack_info: &StackInfo,
    branch_name: &str,
    model: &str,
    budget: f64,
) -> Result<BatchOutput> {
    let num_groups = batch_groups.len();
    let per_group_budget = budget / num_groups as f64;

    // Shared atomic cost tracker so groups can see aggregate spending.
    let shared_cost = Arc::new(AtomicU64::new(0));

    let mut join_set: JoinSet<Result<GroupOutput>> = JoinSet::new();

    for (group_idx, task_indices) in batch_groups.iter().enumerate() {
        // Clone / own everything the spawned task needs.
        let clone_path = clone_path.to_path_buf();
        let task_indices = task_indices.clone();
        let tasks: Vec<Improvement> = task_indices
            .iter()
            .map(|&i| improvements[i].clone())
            .collect();
        let stack_info = stack_info.clone();
        let model = model.to_string();
        let shared_cost = Arc::clone(&shared_cost);

        join_set.spawn(async move {
            let worktree_path =
                create_worktree(&clone_path, &format!("batch-group-{group_idx}")).await?;

            let output = run_group_in_worktree(
                &worktree_path,
                &tasks,
                &task_indices,
                &stack_info,
                &model,
                per_group_budget,
                &shared_cost,
            )
            .await;

            // Always try to generate a patch before cleanup, even on error.
            let patch = if output.as_ref().map_or(false, |o| {
                o.results.iter().any(|r| matches!(r.status, TaskStatus::Success))
            }) {
                generate_patch(&worktree_path).await.ok()
            } else {
                None
            };

            // Cleanup worktree.
            if let Err(e) = remove_worktree(&clone_path, &worktree_path).await {
                warn!(error = %e, "failed to remove worktree");
            }

            let mut group_output = output?;
            group_output.patch = patch;
            Ok(group_output)
        });
    }

    // Collect all group outputs.
    let mut group_outputs: Vec<GroupOutput> = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(Ok(output)) => group_outputs.push(output),
            Ok(Err(e)) => {
                warn!(error = %e, "group execution failed");
            }
            Err(e) => {
                warn!(error = %e, "group task panicked");
            }
        }
    }

    // Merge patches back into main clone and push.
    merge_and_push(clone_path, branch_name, &mut group_outputs).await?;

    // Aggregate results.
    let mut all_results: Vec<TaskResult> = Vec::new();
    let mut total_cost: f64 = 0.0;
    for output in group_outputs {
        total_cost += output.cost_usd;
        all_results.extend(output.results);
    }

    // Sort results by title for deterministic output.
    all_results.sort_by_key(|r| r.title.clone());

    Ok(BatchOutput {
        results: all_results,
        total_cost_usd: total_cost,
    })
}

// ---------------------------------------------------------------------------
// Group execution (sequential tasks within a single worktree)
// ---------------------------------------------------------------------------

struct GroupOutput {
    results: Vec<TaskResult>,
    cost_usd: f64,
    patch: Option<String>,
    successful_titles: Vec<String>,
}

async fn run_group_in_worktree(
    worktree_path: &Path,
    tasks: &[Improvement],
    _task_indices: &[usize],
    stack_info: &StackInfo,
    model: &str,
    group_budget: f64,
    shared_cost: &Arc<AtomicU64>,
) -> Result<GroupOutput> {
    let mut results: Vec<TaskResult> = Vec::new();
    let mut group_cost: f64 = 0.0;
    let mut successful_titles: Vec<String> = Vec::new();
    let total_tasks = tasks.len();

    for (idx, improvement) in tasks.iter().enumerate() {
        let remaining_tasks = total_tasks - idx;
        let remaining_budget = group_budget - group_cost;
        let per_task_budget = (remaining_budget / remaining_tasks as f64).min(group_budget * 0.30);

        if per_task_budget <= 0.0 {
            warn!(task = %improvement.title, "budget exhausted, skipping remaining tasks");
            results.push(TaskResult {
                title: improvement.title.clone(),
                status: TaskStatus::Skipped("budget exhausted".to_string()),
                cost_usd: 0.0,
                files_changed: vec![],
            });
            continue;
        }

        info!(
            task = %improvement.title,
            index = idx + 1,
            total = total_tasks,
            budget = per_task_budget,
            "starting implementation task in worktree"
        );

        let task_result = run_single_task(
            worktree_path,
            improvement,
            stack_info,
            model,
            per_task_budget,
        )
        .await;

        match task_result {
            Ok(result) => {
                let cost = result.cost_usd;
                // Track cost in the shared atomic counter (stored as microdollars).
                let microdollars = (cost * 1_000_000.0) as u64;
                shared_cost.fetch_add(microdollars, Ordering::Relaxed);
                group_cost += cost;

                if matches!(result.status, TaskStatus::Success) {
                    successful_titles.push(result.title.clone());
                }
                results.push(result);
            }
            Err(e) => {
                warn!(task = %improvement.title, error = %e, "task execution failed");
                // Discard changes from this failed task, but continue with the rest.
                let _ = guardrails::discard_changes(worktree_path);
                results.push(TaskResult {
                    title: improvement.title.clone(),
                    status: TaskStatus::Failed(format!("task error: {e}")),
                    cost_usd: 0.0,
                    files_changed: vec![],
                });
            }
        }
    }

    Ok(GroupOutput {
        results,
        cost_usd: group_cost,
        patch: None, // filled in by caller after this returns
        successful_titles,
    })
}

// ---------------------------------------------------------------------------
// Single-task execution (prompt, invoke, validate, build-check, fix)
// ---------------------------------------------------------------------------

async fn run_single_task(
    working_dir: &Path,
    improvement: &Improvement,
    stack_info: &StackInfo,
    model: &str,
    per_task_budget: f64,
) -> Result<TaskResult> {
    // Step 1: Verify clean state.
    let status_output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(working_dir)
        .output()
        .await
        .context("failed to run git status")?;

    let status_str = String::from_utf8_lossy(&status_output.stdout);
    if !status_str.trim().is_empty() {
        warn!(task = %improvement.title, "working tree not clean before task, discarding stale changes");
        guardrails::discard_changes(working_dir)?;
    }

    // Step 2: Build implementation prompt.
    let category_str = match improvement.category {
        Category::Bug => "bug",
        Category::Performance => "performance",
        Category::Security => "security",
        Category::Quality => "quality",
        Category::Testing => "testing",
        Category::Docs => "docs",
        Category::ErrorHandling => "error-handling",
    };

    let allowed_files = improvement.files_to_modify.join("\n");
    let build_command = stack_info
        .build_commands
        .first()
        .map(|s| s.as_str())
        .unwrap_or("N/A");
    let test_command = stack_info
        .test_commands
        .first()
        .map(|s| s.as_str())
        .unwrap_or("N/A");

    let prompt = IMPLEMENT_PROMPT
        .replace("{task_title}", &improvement.title)
        .replace("{task_description}", &improvement.description)
        .replace("{task_category}", category_str)
        .replace("{allowed_files}", &allowed_files)
        .replace("{primary_language}", &stack_info.primary_language)
        .replace("{build_command}", build_command)
        .replace("{test_command}", test_command);

    // Step 3: Invoke Claude.
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(implement_system_prompt()),
        model: model.to_string(),
        max_budget_usd: per_task_budget,
        max_turns: 100,
        effort: "high",
        tools: "Read,Glob,Grep,Bash,Edit,Write",
        json_schema: None,
        working_dir: working_dir.to_path_buf(),
        session_id: Some(claude::generate_session_id()),
        resume_session_id: None,
    };

    let response: ClaudeResponse<serde_json::Value> =
        match claude::invoke(&invocation, TASK_TIMEOUT).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!(
                    task = %improvement.title,
                    error = %e,
                    "claude invocation failed, skipping task"
                );
                guardrails::discard_changes(working_dir)?;
                return Ok(TaskResult {
                    title: improvement.title.clone(),
                    status: TaskStatus::Failed(format!("claude invocation failed: {e}")),
                    cost_usd: 0.0,
                    files_changed: vec![],
                });
            }
        };

    let task_cost = response.cost_usd;

    // Step 4: Validate scope via guardrails.
    info!(task = %improvement.title, "validating diff against guardrails");
    match guardrails::validate_diff(working_dir, &improvement.files_to_modify, 500, false) {
        Ok(diff_report) => {
            info!(
                task = %improvement.title,
                files_changed = ?diff_report.files_changed,
                lines_added = diff_report.lines_added,
                lines_removed = diff_report.lines_removed,
                "diff validation passed"
            );

            // Stage all changes (no build check -- CI will verify after push).
            stage_all(working_dir).await?;

            Ok(TaskResult {
                title: improvement.title.clone(),
                status: TaskStatus::Success,
                cost_usd: task_cost,
                files_changed: diff_report.files_changed,
            })
        }
        Err(violation) => {
            warn!(
                task = %improvement.title,
                violation = %violation,
                "guardrail violation, discarding changes and skipping task"
            );
            guardrails::discard_changes(working_dir)?;
            Ok(TaskResult {
                title: improvement.title.clone(),
                status: TaskStatus::Skipped(format!("guardrail violation: {violation}")),
                cost_usd: task_cost,
                files_changed: vec![],
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Merge patches from worktrees back into the main clone, then push
// ---------------------------------------------------------------------------

async fn merge_and_push(
    clone_path: &Path,
    branch_name: &str,
    group_outputs: &mut [GroupOutput],
) -> Result<()> {
    let mut all_successful_titles: Vec<String> = Vec::new();
    let mut any_applied = false;

    for output in group_outputs.iter_mut() {
        let patch = match &output.patch {
            Some(p) if !p.trim().is_empty() => p.clone(),
            _ => continue,
        };

        info!(
            titles = ?output.successful_titles,
            "applying patch from worktree group"
        );

        // Write patch to a temp file.
        let patch_file = clone_path.join(".autoanneal-patch.tmp");
        tokio::fs::write(&patch_file, &patch)
            .await
            .context("failed to write patch file")?;

        let apply_output = tokio::process::Command::new("git")
            .args(["apply", "--3way", ".autoanneal-patch.tmp"])
            .current_dir(clone_path)
            .output()
            .await
            .context("failed to run git apply")?;

        // Clean up temp file.
        let _ = tokio::fs::remove_file(&patch_file).await;

        if !apply_output.status.success() {
            let stderr = String::from_utf8_lossy(&apply_output.stderr);
            warn!(
                stderr = %stderr,
                "patch apply failed, marking group tasks as failed"
            );
            // Mark all successful results in this group as failed.
            for result in &mut output.results {
                if matches!(result.status, TaskStatus::Success) {
                    result.status =
                        TaskStatus::Failed(format!("patch apply failed: {stderr}"));
                    result.files_changed.clear();
                }
            }
            continue;
        }

        any_applied = true;
        all_successful_titles.extend(output.successful_titles.clone());
    }

    if !any_applied {
        info!("no patches to commit");
        return Ok(());
    }

    // Stage everything and commit.
    stage_all(clone_path).await?;

    let commit_msg = if all_successful_titles.len() == 1 {
        format!(
            "autoanneal: {}\n\nAutomated by autoanneal",
            all_successful_titles[0]
        )
    } else {
        let titles_list: String = all_successful_titles
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "autoanneal: implement {} improvements\n\n{titles_list}\n\nAutomated by autoanneal",
            all_successful_titles.len()
        )
    };

    let commit_output = tokio::process::Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to run git commit")?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        warn!(stderr = %stderr, "git commit failed after patch merge");
        return Err(anyhow::anyhow!("git commit failed: {stderr}"));
    }

    info!(branch = %branch_name, "committed merged changes (push deferred until after review)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Worktree helpers
// ---------------------------------------------------------------------------

/// Create a git worktree branching from the current HEAD of `repo_dir`.
/// Returns the absolute path to the new worktree directory.
async fn create_worktree(repo_dir: &Path, name: &str) -> Result<PathBuf> {
    let parent_dir = repo_dir.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot create worktree: repo_dir '{}' has no parent directory",
            repo_dir.display()
        )
    })?;
    let worktree_dir = parent_dir.join(format!(".autoanneal-worktree-{name}"));

    // Remove stale worktree if it exists.
    if worktree_dir.exists() {
        let _ = remove_worktree(repo_dir, &worktree_dir).await;
    }

    // Get current HEAD ref.
    let head_output = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .await
        .context("failed to get HEAD ref")?;

    let head_ref = String::from_utf8_lossy(&head_output.stdout).trim().to_string();

    let worktree_dir_str = worktree_dir
        .to_str()
        .context("worktree path contains non-UTF8 characters")?;
    let output = tokio::process::Command::new("git")
        .args(["worktree", "add", "--detach", worktree_dir_str, &head_ref])
        .current_dir(repo_dir)
        .output()
        .await
        .context("failed to create git worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }

    info!(path = %worktree_dir.display(), "created git worktree");
    Ok(worktree_dir)
}

/// Remove a git worktree and its directory.
async fn remove_worktree(repo_dir: &Path, worktree_path: &Path) -> Result<()> {
    let worktree_path_str = worktree_path
        .to_str()
        .context("worktree path contains non-UTF8 characters")?;
    let output = tokio::process::Command::new("git")
        .args(["worktree", "remove", "--force", worktree_path_str])
        .current_dir(repo_dir)
        .output()
        .await
        .context("failed to remove git worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "git worktree remove failed, cleaning up manually");
        let _ = tokio::fs::remove_dir_all(worktree_path).await;
        // Prune stale worktree entries.
        let _ = tokio::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_dir)
            .output()
            .await;
    }

    info!(path = %worktree_path.display(), "removed git worktree");
    Ok(())
}

/// Generate a unified diff of all staged changes in the worktree relative to HEAD.
async fn generate_patch(worktree_path: &Path) -> Result<String> {
    // Make sure everything is staged.
    stage_all(worktree_path).await?;

    let output = tokio::process::Command::new("git")
        .args(["diff", "--cached", "HEAD"])
        .current_dir(worktree_path)
        .output()
        .await
        .context("failed to generate patch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --cached HEAD failed: {stderr}");
    }

    let patch = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(patch)
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Stage all changes in the working directory.
async fn stage_all(dir: &Path) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output()
        .await
        .context("failed to run git add -A")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git add -A failed: {}", stderr);
    }

    Ok(())
}

