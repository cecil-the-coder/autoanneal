# Architecture

## Phase pipeline

All phases run sequentially. Each produces typed output consumed by subsequent phases via `RunState`, a struct that accumulates results across the run.

```
Preflight + Recon ──> Analysis ──> Plan + PR ──> Implement ──> [Critic] ──> [CI Watch]
     │                   │             │              │
  RepoInfo          Improvements   PR number      TaskResults
  StackInfo                        Branch name
  ArchSummary
```

Phases marked `[ ]` are deferred to v2.

## Phase details

### Phase 1: Preflight + Recon

No Claude calls for preflight. Pure validation:

1. Check `ANTHROPIC_API_KEY` and `GH_TOKEN` are set.
2. `gh auth status` — fail fast on invalid token.
3. Fetch repo metadata via `gh repo view` (archived status, permissions, disk usage, default branch).
4. Reject archived repos and repos where the token lacks write permission.
5. Clone: shallow (`--depth 1`) if disk usage > 500 MB, full clone otherwise.
6. Disable git hooks: `git config core.hooksPath /dev/null`.
7. Set git identity to `autoworker@github.com` / `autoanneal`.
8. Run `--setup-command` if provided (5 min timeout).
9. Detect tech stack by scanning for `package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `pom.xml`, etc.
10. Detect CI by scanning `.github/workflows/*.yml`.
11. Fetch open PRs to avoid duplicate work.

Then a single Claude invocation produces an architecture summary (2-3 paragraphs), identifies the primary language, and extracts build/test/lint commands.

### Phase 2: Analysis

Claude explores the codebase with read-only tools (`Read`, `Glob`, `Grep`, `Bash`) and returns structured JSON: a list of improvements, each with title, description, severity, category, affected files, estimated line count, and risk level.

Post-processing in Rust:
- Filter out `risk: "high"` items.
- Filter out items with `estimated_lines_changed > 500`.
- Apply `--min-severity` filter.
- Sort by severity (descending), then risk (ascending).
- Truncate to `--max-tasks`.
- If nothing remains: exit 0 with "No actionable improvements found".

### Phase 3: Plan + PR

1. Generate branch name: `autoworker/<date>-<short-hash>` (hash derived from improvements JSON).
2. Create and push branch.
3. Claude generates a PR title and markdown body (no tools, single turn).
4. Open draft PR via `gh pr create --draft`.

### Phase 4: Implement

For each improvement task, in order:

1. **Verify clean state** — `git status --porcelain` must be empty.
2. **Claude implements** — Full tool access (`Read`, `Glob`, `Grep`, `Bash`, `Edit`, `Write`). Side-effect-based; no structured output required.
3. **Validate scope** — Parse `git diff --numstat`. Reject if:
   - Files outside the planned allowlist were touched (tolerance: 2 extra files).
   - Total lines changed exceed 500.
   - Files were deleted without explicit plan.
   - On rejection: `git checkout . && git clean -fd`, skip task.
4. **Build check** — Run the first build command from recon (2 min timeout). On failure: Claude gets two fix attempts ($0.50 each). Still failing: revert and skip.
5. **Commit and push** — `git add -A`, commit with structured message, `git push --force-with-lease`.

### Phase 4.5: Critic Review (v2)

A separate Claude invocation reviews the full diff as a skeptical reviewer. Scores the PR 1-10. Below `--critic-threshold`: PR stays as draft with a review comment.

### Phase 5: CI Watch + Fix (v2)

Polls CI via `gh run list` / `gh run watch`. On failure: extracts logs, invokes Claude to fix, up to `--ci-retries` attempts. On success: `gh pr ready`.

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
| Preflight + Recon | 5 min | $0.50 | Claude call: low effort, 10 turns max |
| Analysis | 5 min | 20% of remaining (min $0.50) | High effort, 25 turns max |
| Plan + PR | 2 min | $0.10 | Low effort, 1 turn, no tools |
| Implement (per task) | 15 min total | remaining / tasks (cap $1.50/task) | High effort, 20 turns max |
| Build fix (per attempt) | — | $0.50 | Up to 2 attempts per task |

Global caps: `--max-budget` (default $5) and `--timeout` (default 30 min) apply across all phases.

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
