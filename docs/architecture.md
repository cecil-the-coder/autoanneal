# Architecture

## Overview

autoanneal uses a **concurrent work queue** architecture. After a shared Preflight and Recon phase, the orchestrator builds a queue of heterogeneous work items and executes them concurrently using a `JoinSet`, bounded by `--concurrency` slots (default 3). Each concurrent work item receives its own **git worktree** for filesystem isolation.

```
Preflight ──> Recon ──> Early-exit checks ──> Build work queue ──> Concurrent execution (JoinSet)
               │                                      │                        │
          RepoInfo                            ┌───────┴───────┐          ┌──────┴──────┐
          StackInfo                           │               │          │             │
          ArchSummary                     CiFix items    PrReview items  IssueInv.   Analysis
          open PRs                         (worktree)     (worktree)    (worktree)   (main clone)
                                                                                      │
                                                                           Analysis→Plan→Implement→Critic→PR
```

### Early-exit checks (before work queue)

After Preflight, before Recon costs money:

1. **PR limit** — If `--max-open-prs > 0` and in-flight autoanneal PRs ≥ limit, skip unless there is maintenance/review/issue work to do.
2. **Staleness** — If `--skip-after > 0` and no external work items exist: compute threshold as `skip_after × cron_interval × 60` seconds. If the newest commit across all branches is older than this threshold, skip the run entirely.

## Work item types

The orchestrator (`orchestrator.rs`) defines four `WorkItemKind` variants, each with its own budget cap and execution logic:

### 1. CiFix — Fix failing CI or merge conflicts on existing PRs

**Triggered when:** `--fix-ci` is enabled and in-flight PRs have failing CI, or `--fix-conflicts` is enabled and in-flight PRs have merge conflicts (excluding PRs already labeled `autoanneal:fixing`).

**Execution:**
1. Creates a git worktree at the PR's branch via `WorktreeManager::create_at_branch`.
2. Adds `autoanneal:fixing` label to the PR (removed on drop via `FixingLabelGuard`).
3. For merge conflicts: attempts `git merge origin/{default_branch}`. If clean, pushes the merge commit directly (no Claude). If conflicts remain, invokes Claude to resolve them.
4. For CI failures: fetches failed CI logs via `gh run view --log-failed`, invokes Claude with the logs to produce a fix.
5. Commits and pushes any changes back to the PR branch.
6. Removes the worktree.

**Budget cap:** min(remaining, $2.00).

### 2. PrReview — Review external PRs with optional auto-fix

**Triggered when:** `--review-prs` is enabled. Considers up to 3 external PRs (non-autoanneal), filtered by `--review-filter` (`all`, `labeled:<label>`, or `recent`).

**Execution:**
1. Creates a git worktree at the PR's branch.
2. Fetches the PR diff via `gh pr diff` (truncated to 50,000 characters).
3. Runs the critic prompt on the diff to produce a score (1–10) and summary.
4. If score ≥ `--review-fix-threshold` (default 7): adds `autoanneal:reviewed` label, no changes needed.
5. If score < threshold: invokes Claude with a fix prompt to address the issues. If changes are produced, commits and pushes to the PR branch.
6. Leaves a comment on the PR with the review summary. Adds `autoanneal:reviewed` label.
7. Removes the worktree.

**Budget cap:** min(remaining, $2.00).

### 3. IssueInvestigation — Investigate labeled GitHub issues

**Triggered when:** `--investigate-issues` is set to a comma-separated label list. Up to `--max-issues` (default 2) per run.

**Execution:**
1. Creates a git worktree from HEAD.
2. Adds `autoanneal:investigating` label to the issue.
3. Invokes Claude with the issue body, architecture summary, and build/test commands. Claude has full tool access to explore the codebase and produce a fix.
4. If Claude reports `fixed: true`: creates a branch (`autoanneal/issue-{number}-{timestamp}`), commits, pushes, and opens a PR referencing the issue.
5. If not fixed: leaves a comment on the issue with the investigation findings.
6. Replaces `autoanneal:investigating` label with `autoanneal:attempted`.
7. Removes the worktree.

**Budget cap:** min(remaining, `--issue-budget`).

### 4. Analysis — The main improvement pipeline

**Triggered when:** budget remains and the run is not at the PR limit (skipped if `--max-open-prs` is reached). The analysis pipeline uses the main clone (not a worktree) since it needs to push branches.

**Sub-phases (sequential within this work item):**

#### Analysis sub-phase

Claude explores the codebase with read-only tools (`Read`, `Glob`, `Grep`, `Bash`) and returns structured JSON: a list of improvements, each with title, description, severity, category, affected files, estimated line count, and risk level.

Post-processing in Rust:
- Filter out `risk: "high"` items.
- Filter out items with `estimated_lines_changed > 500`.
- Apply `--min-severity` filter.
- Sort by severity (descending), then risk (ascending).
- Truncate to `--max-tasks`.
- If nothing remains and `--improve-docs` is enabled: **documentation fallback** — run a second analysis pass focused on documentation improvements (README, API docs, architecture docs). If that also returns nothing: exit with no PR.
- If nothing remains and `--improve-docs` is disabled: exit with no PR.

#### Plan sub-phase

1. Generate branch name: `autoworker/<date>-<short-hash>` (hash derived from improvements JSON).
2. Create branch locally.

#### Implement sub-phase

For each improvement task, in order:

1. **Verify clean state** — `git status --porcelain` must be empty.
2. **Claude implements** — Full tool access (`Read`, `Glob`, `Grep`, `Bash`, `Edit`, `Write`). Side-effect-based; no structured output required.
3. **Validate scope** — Parse `git diff --numstat`. Reject if:
   - Files outside the planned allowlist were touched (tolerance: `max(2, allowed_files.len() × 0.2)` extra files).
   - Total lines changed exceed 500.
   - Files were deleted without explicit plan.
   - On rejection: `git checkout . && git clean -fd`, skip task.
4. **Build check** — Run the first build command from recon (2 min timeout). On failure: Claude gets two fix attempts ($0.50 each). Still failing: revert and skip.
5. **Commit** — `git add -A`, commit with structured message.

#### Critic sub-phase

A separate Claude invocation reviews the full diff as a skeptical reviewer. Scores the PR 1–10. Uses `--doc-critic-threshold` for documentation-only PRs (higher bar), `--critic-threshold` for code PRs. Below threshold: branch is pushed but no PR is created; cleanup guard deletes the branch.

#### Push + PR creation

Only after critic approval:
1. `git push -u origin <branch> --force-with-lease`.
2. Claude generates a PR title and markdown body (no tools, single turn).
3. Open draft PR via `gh pr create --draft`.

## Worktree isolation

Implemented in `worktree.rs`. The `WorktreeManager` creates isolated git worktrees off the canonical clone directory:

- **`create_from_head(name)`** — Creates a detached worktree at HEAD. Used by IssueInvestigation.
- **`create_at_branch(name, branch)`** — Creates a worktree, fetches the remote branch, and checks it out. Used by CiFix and PrReview.
- **`remove(path)`** — Force-removes the worktree and prunes.

Each worktree gets its own git identity (`autoanneal[bot]`). Worktrees are cleaned up after the work item completes, even on failure.

## Concurrency model

The `run_work_queue` function in `orchestrator.rs`:

1. Fills initial slots up to `--concurrency` (minimum 1).
2. As each `JoinSet` task completes, frees a slot and spawns the next pending item.
3. Each work item runs in its own `tokio::spawn` task with its own worktree (or the main clone for Analysis).
4. Outcomes are collected and processed sequentially after all items complete.

Budget is shared across all concurrent items. Actual costs are subtracted when outcomes are processed (not pre-reserved), so items may exceed their nominal cap if many run concurrently.

## Shared phases (Preflight and Recon)

### Phase 1: Preflight

No Claude calls. Pure validation:

1. Check `ANTHROPIC_API_KEY` and `GH_TOKEN` are set.
2. `gh auth status` — fail fast on invalid token.
3. Fetch repo metadata via `gh repo view` (archived status, permissions, disk usage, default branch).
4. Reject archived repos and repos where the token lacks write permission.
5. Fetch in-flight autoanneal PRs, external PRs (if `--review-prs`), and labeled issues (if `--investigate-issues`).
6. Compute newest commit age across all branches for staleness check.

### Phase 2: Recon

Single Claude invocation (5% of `--max-budget`, i.e. $0.25 at default $5; low effort; 25 turns max) produces an architecture summary (2-3 paragraphs), identifies the primary language, and extracts build/test/lint commands. Also:

1. Clone: shallow (`--depth 1`) if disk usage > 500 MB, full clone otherwise.
2. Disable git hooks: `git config core.hooksPath /dev/null`.
3. Set git identity to `autoanneal[bot]` / `autoanneal[bot]@users.noreply.github.com`.
4. Run `--setup-command` if provided (5 min timeout).
5. Detect tech stack by scanning for `package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `pom.xml`, etc.
6. Detect CI by scanning `.github/workflows/*.yml`.
7. Fetch open PRs to avoid duplicate work.

## Claude invocation details

Every Claude call is constructed with these base flags:

```
claude -p "<prompt>"
  --output-format json
  --bare
  --dangerously-skip-permissions
  --no-session-persistence
  --model <model>
  --max-budget-usd <budget>
  --max-turns <turns>
  --effort <effort>
  --tools "<tools>"
```

When structured output is needed, `--json-schema '<schema>'` is appended.

### Response envelope

```json
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "duration_ms": 12345,
  "num_turns": 5,
  "result": "text content...",
  "total_cost_usd": 0.42,
  "session_id": "uuid",
  "usage": { },
  "structured_output": { }
}
```

Key fields:
- `is_error` — Check first. If true, the invocation failed.
- `subtype` — `"success"`, `"error_max_turns"`, or `"error_budget"`.
- `result` — Claude's text output (always present).
- `structured_output` — Typed JSON when `--json-schema` was used.
- `total_cost_usd` — Used for budget tracking.

### Error handling

| Condition | Detection | Action |
|-----------|-----------|--------|
| Success | `is_error: false`, `subtype: "success"` | Parse `result` / `structured_output` |
| Auth failure | `is_error: true`, result contains "Not logged in" | Fatal, abort run |
| Budget exhausted | `subtype: "error_budget"` | Treat as partial success, extract available output |
| Max turns hit | `subtype: "error_max_turns"` | Treat as partial (Claude may have done useful work) |
| Process timeout | `tokio::time::timeout` fires | Kill child process, fail phase |
| Malformed JSON | `serde_json::from_str` fails | Log raw output, retry once, then fail |
| Non-zero exit | Exit code != 0 | Parse stderr, retry once, then fail |

### Retries

- Claude invocations: retry once on transient errors (malformed output, non-zero exit without auth failure).
- `gh` CLI calls: retry 3x with exponential backoff (1s, 2s, 4s). Immediate failure on 401. Sleep until reset on 403 rate-limit. Retry on 5xx.

## Budget and timeout allocation

| Work item / Phase | Timeout | Budget | Notes |
|-------------------|---------|--------|-------|
| Preflight | 60s | $0 | No Claude calls |
| Recon | 5 min | 5% of `--max-budget` ($0.25 at default $5) | Low effort, 25 turns max |
| **CiFix** | 15 min | min(remaining, $2.00) | High effort, 100 turns max |
| **PrReview** | 5 min (critic) + 10 min (fix) | min(remaining, $2.00) | 30% for critic, 70% for fix |
| **IssueInvestigation** | 15 min | min(remaining, `--issue-budget`) | High effort, 100 turns max |
| **Analysis** sub-phase | 10 min | 20% of remaining (min $0.50) | High effort, 25 turns max |
| **Analysis** doc fallback | 10 min | 20% of remaining (min $0.50) | Only if code analysis yields nothing and `--improve-docs` is on |
| **Analysis** implement | 30 min | 60% of remaining | Per-task budget: remaining / tasks (cap $1.50/task) |
| **Analysis** critic | 5 min | min(remaining, $1.50) | Separate threshold for doc PRs |
| **Analysis** PR creation | 2 min | min(remaining, $0.10) | Low effort, 1 turn, no tools |

Global caps: `--max-budget` (default $5) and `--timeout` (default 30 min) apply across all phases and work items. Budget is shared across concurrent items; actual costs are deducted when outcomes are processed.

## Guardrails

Implemented in `guardrails.rs`. After each task implementation, `validate_diff()` parses `git diff --numstat` and enforces:

- **File allowlist**: Only files listed in the improvement's `files_to_modify` may be changed. Tolerance: `max(2, allowed_files.len() * 0.2)` extra files.
- **Line count cap**: No more than 500 lines changed per task.
- **Deletion control**: File deletions rejected unless explicitly planned.

Violations cause the task to be reverted (`git checkout . && git clean -fd`) and skipped.

## Cleanup behavior

An RAII `CleanupGuard` struct runs on drop (including panics and signals):

| State | Action |
|-------|--------|
| PR exists, no successful tasks | `gh pr close <n> --delete-branch` |
| PR exists, some tasks succeeded | Leave as draft (partial work has value) |
| Branch exists, no PR | `git push origin --delete <branch>` |

Cleanup is skipped when `--keep-on-failure` is set. SIGTERM/SIGINT are caught via `tokio::signal::ctrl_c()` to trigger cleanup before exit.

## Deployment options

autoanneal supports two deployment models:

- **Docker (single run)** -- Run the container directly for one-off or ad-hoc repo analysis. See [docker.md](docker.md).
- **Kubernetes via Helm (scheduled)** -- The Helm chart in `charts/autoanneal/` creates a CronJob per repo entry. Each CronJob runs on its own cron schedule with independent configuration (budget, model, timeout, setup commands). This is the recommended approach for ongoing, unattended operation across multiple repos. One-off Jobs are also supported for immediate runs. See [kubernetes.md](kubernetes.md).

The CronJob-per-repo pattern ensures that repos are processed independently: a failure or timeout in one repo does not block others, and schedules can be tuned per repo (e.g., weekly for stable services, monthly for low-churn repos).

## Safety measures

**Security:**
- Git hooks disabled after clone (`core.hooksPath /dev/null`) to prevent hook-based exfiltration.
- Docker image runs as non-root `worker` user.
- Recommended: `--cap-drop=ALL`, `--security-opt=no-new-privileges`.
- GitHub token should be a fine-grained PAT scoped to the target repo.
- PRs are always created as drafts. Human review is required before merge.

**Scope control:**
- Per-task file allowlist derived from the analysis phase.
- 500-line cap per task.
- No unplanned file deletions.
- Build verification after every task, with up to two automated fix attempts.

**Budget and time:**
- Per-phase budget allocation with hard caps.
- Per-phase timeouts via `tokio::time::timeout`.
- `--max-turns` on every Claude invocation prevents infinite loops.
- `--force-with-lease` on all pushes detects external concurrent changes.

**Pre-flight validation:**
- Token validity and scope verified before any work begins.
- Repo must not be archived, must not be empty, token must have write permission.
- Default branch detected dynamically (not hardcoded to `main`).
- Disk space checked (2x repo size required).
