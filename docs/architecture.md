# Architecture

## High-level flow

1. **Preflight** validates the environment and collects repo state (open PRs, external PRs, issues, CI-failing branches).
2. **Recon** clones the repo and produces an architecture summary plus tech-stack detection.
3. A **work queue** is built from prioritized work items (CI fixes > PR reviews > issue investigations > new analysis).
4. Work items execute **concurrently** via a `JoinSet`, each in its own **git worktree** for isolation.
5. Each item type runs its own phase pipeline independently.

```
Preflight ──> Recon ──> collect_work_items() ──> run_work_queue()
                                                       │
                      ┌────────────────────────────────┤
                      │          Concurrent JoinSet     │
                      │                                │
                   CI Fix      PR Review    Issue Inv.   Analysis
                     │            │            │           │
                  (worktree)  (worktree)   (worktree)  (clone dir)
                     │            │            │           │
                   fix+push   review+fix   investigate  Analysis→Plan→
                     │            │            │        Implement→Critic→PR
                   clean up    clean up     clean up      │
                      │            │            │        clean up
                      └────────────┴────────────┴─────────┘
                                  outcomes aggregated
```

## Concurrent work queue model

Work items are represented as `WorkItem` enums and executed through a bounded-concurrency `tokio::task::JoinSet` (controlled by `--concurrency`, default 3). Each work item gets an independent budget cap. The `collect_work_items()` function builds the queue with this priority order:

1. **CI fixes** — autoanneal PRs with failing CI or merge conflicts (`--fix-ci`, `--fix-conflicts`).
2. **PR reviews** — external PRs not created by autoanneal (`--review-prs`, `--review-filter`).
3. **Issue investigations** — open GitHub issues matching a label filter (`--investigate-issues`, `--max-issues`).
4. **Analysis pipeline** — the original analysis→plan→implement→critic→PR flow (skipped if `--max-open-prs` is reached).

Budget is not pre-reserved; actual costs are deducted when outcomes are processed, which allows sharing the global budget fairly across concurrent items.

## Git worktree isolation

Implemented in `src/worktree.rs`. The `WorktreeManager` creates detached git worktrees from the canonical clone, giving each concurrent work item its own isolated working directory. This avoids lock contention and allows parallel git operations.

- `create_from_head(name)` — creates a worktree at HEAD (used for issue investigations).
- `create_at_branch(name, remote_branch)` — creates a worktree, fetches a remote branch, and checks it out (used for CI fixes and PR reviews).
- `remove(path)` — cleans up the worktree via `git worktree remove --force` plus `git worktree prune`.

Worktrees are created in `/tmp/autoanneal-<timestamp>/.worktree-<name>` and removed after the work item completes.

## Phase details

### Phase 1: Preflight

No Claude calls. Pure validation and data collection:

1. Check `ANTHROPIC_API_KEY` and `GH_TOKEN` are set.
2. `gh auth status` — fail fast on invalid token.
3. Fetch repo metadata via `gh repo view` (archived status, permissions, disk usage, default branch).
4. Reject archived repos and repos where the token lacks write permission.
5. Fetch open PRs to avoid duplicate work.
6. Identify autoanneal in-flight PRs needing CI fixes or merge conflict resolution.
7. Fetch external PRs eligible for review (filtered by `--review-filter`: `all`, `labeled:<label>`, or `recent`).
8. Fetch open GitHub issues matching the `--investigate-issues` label filter.
9. Check staleness: skip if no recent commits and no maintenance/review/issue work.

### Phase 2: Recon

1. Clone: shallow (`--depth 1`) if disk usage > 500 MB, full clone otherwise.
2. Disable git hooks: `git config core.hooksPath /dev/null`.
3. Set git identity to `autoanneal[bot]` / `autoanneal[bot]@users.noreply.github.com`.
4. Run `--setup-command` if provided (5 min timeout).
5. Detect tech stack by scanning for `package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `pom.xml`, etc.
6. Detect CI by scanning `.github/workflows/*.yml`.
7. A single Claude invocation produces an architecture summary (2-3 paragraphs), identifies the primary language, and extracts build/test/lint commands.

### Phase 3: CI Fix

Each CI fix runs in its own worktree checked out at the PR's branch:

1. Add `autoanneal:fixing` label to the PR (with a drop guard that removes it on exit).
2. If merge conflicts: fetch and merge the default branch. If clean merge, push and done (no Claude needed).
3. If CI failure or unresolved conflicts: fetch CI logs via `gh run view --log-failed` or conflict diff.
4. Invoke Claude with full tool access to fix the issue (15 min timeout, high effort, up to 100 turns).
5. Commit (`autoanneal: fix CI failures`) and push.

Budget cap: $2.00 per fix. Controlled by `--fix-ci` and `--fix-conflicts` flags.

### Phase 4: PR Review

Reviews external (non-autoanneal) PRs in isolated worktrees:

1. Fetch PR diff via `gh pr diff` (truncated at 50,000 characters).
2. Run the critic model on the diff — produces a score (1-10), verdict, and summary.
3. If score ≥ `--review-fix-threshold` (default 7): add `autoanneal:reviewed` label and skip.
4. If score < threshold and budget allows: invoke Claude with a fix prompt to address issues directly in the worktree.
5. If Claude made changes: commit and push to the PR branch; leave a comment summarizing fixes.
6. If push fails (protected branch / no permission): leave a review comment with suggestions instead.
7. If no changes needed: leave a comment with the review summary.
8. Add `autoanneal:reviewed` label.

Budget cap: $2.00 per review. Controlled by `--review-prs` and `--review-filter` flags.

### Phase 5: Issue Investigation

Investigates open GitHub issues matching a configurable label:

1. Add `autoanneal:investigating` label to the issue.
2. Invoke Claude with the issue title, body, architecture summary, and build/test commands.
3. Claude explores the codebase and attempts to produce a fix (15 min timeout, high effort, up to 100 turns).
4. If fix produced: create branch (`autoanneal/issue-<number>-<timestamp>`), commit, push, and open a PR referencing the issue.
5. If no fix: leave a comment on the issue with investigation findings.
6. Replace `autoanneal:investigating` label with `autoanneal:attempted`.

Budget: `--issue-budget` per investigation (default $3.00), up to `--max-issues` per run (default 2). Controlled by `--investigate-issues` label filter.

### Phase 6: Analysis Pipeline

The original sequential pipeline, now running as a single work item:

#### Step A: Analysis

Claude explores the codebase with read-only tools (`Read`, `Glob`, `Grep`, `Bash`) and returns structured JSON: a list of improvements, each with title, description, severity, category, affected files, estimated line count, and risk level.

Post-processing in Rust:
- Filter out `risk: "high"` items.
- Filter out items with `estimated_lines_changed > 500`.
- Apply `--min-severity` filter.
- Sort by severity (descending), then risk (ascending).
- Truncate to `--max-tasks`.
- If nothing remains and `--improve-docs` is set: fall back to documentation-focused analysis.
- If still nothing: exit 0 with "No actionable improvements found".

#### Step B: Plan + Branch

1. Generate branch name: `autoworker/<date>-<short-hash>` (hash derived from improvements JSON).
2. Create branch locally.

#### Step C: Implement

For each improvement task, in order:

1. **Verify clean state** — `git status --porcelain` must be empty.
2. **Claude implements** — Full tool access (`Read`, `Glob`, `Grep`, `Bash`, `Edit`, `Write`). Side-effect-based; no structured output required.
3. **Validate scope** — Parse `git diff --numstat`. Reject if:
   - Files outside the planned allowlist were touched (tolerance: 2 extra files).
   - Total lines changed exceed 500.
   - Files were deleted without explicit plan.
   - On rejection: `git checkout . && git clean -fd`, skip task.
4. **Build check** — Run the first build command from recon (2 min timeout). On failure: Claude gets two fix attempts ($0.50 each). Still failing: revert and skip.
5. **Commit** — `git add -A`, commit with structured message.

#### Step D: Critic Review

A separate Claude invocation reviews the full diff as a skeptical reviewer. Three-pass process:

1. **Pass 1: Review** (40% of critic budget, read-only tools) — Produces a score (1-10), verdict (`approve`, `needs_work`, `reject`), and summary.
2. **Pass 2: Fix** (35% of critic budget, full tool access) — If verdict is `needs_work` and score is 4-7, Claude attempts to address the issues and commits fixes.
3. **Pass 3: Re-review** (25% of critic budget) — If fixes were applied, re-reviews the updated diff for a new score.

Below `--critic-threshold` (default 6): branch is deleted, no PR created. Below `--doc-critic-threshold` (default 7): same for documentation-only changes. Set to 0 to disable critic.

#### Step E: Push + PR

Only executed after critic approval:

1. Push branch with `--force-with-lease`.
2. Claude generates a PR title and markdown body (no tools, single turn).
3. Open draft PR via `gh pr create --draft` with critic review summary included.

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

| Phase | Timeout | Budget | Notes |
|-------|---------|--------|-------|
| Preflight | 1 min | $0.00 | No Claude calls |
| Recon | 5 min | 5% of total (Claude call) | Low effort, architecture summary |
| Analysis | 10 min | 20% of remaining (min $0.50) | High effort, 25 turns max |
| Plan + PR | 2 min | $0.10 (cap) | Low effort, 1 turn, no tools |
| Implement (per task) | 30 min total | 60% of remaining | High effort, 20 turns max |
| Build fix (per attempt) | — | $0.50 | Up to 2 attempts per task |
| Critic review | 15 min | min(remaining, $1.50) | 3-pass: review (40%) → fix (35%) → re-review (25%) |
| CI Fix (per PR) | 15 min | min(remaining, $2.00) | High effort, up to 100 turns |
| PR Review (per PR) | 10 min | min(remaining, $2.00) | Critic + optional fix attempt |
| Issue Investigation (per issue) | 15 min | min(remaining, `--issue-budget`) | Default $3.00, up to `--max-issues` per run |

Concurrent work items share the global budget; costs are deducted from the remaining total as each item completes. The `--concurrency` flag (default 3) limits how many items run simultaneously.

Global caps: `--max-budget` (default $5) and `--timeout` (default 30 min) apply across all phases and concurrent work items.

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
