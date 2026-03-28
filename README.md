# autoanneal

Autonomous code improvement agent — finds and implements improvements in GitHub repos via draft PRs.

## What it does

autoanneal clones a GitHub repository, uses Claude Code to analyze the codebase for actionable improvements (bugs, performance issues, error handling gaps, etc.), then implements them as commits on a new branch. It opens a draft PR with a summary of all changes. The entire process is budget- and time-bounded, with guardrails that validate scope and run build checks after each change.

## Quick start

```bash
docker run --rm \
  -e ANTHROPIC_API_KEY \
  -e GH_TOKEN \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --memory=4g --cpus=2 \
  ghcr.io/cecil-the-coder/autoanneal owner/repo
```

**Required environment variables:**

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Claude API key |
| `GH_TOKEN` or `GITHUB_TOKEN` | GitHub token with `repo` scope. Prefer a fine-grained PAT scoped to the target repo. |

## Kubernetes

For scheduled, hands-off operation, deploy with the Helm chart. Each repo gets its own CronJob.

```bash
helm install autoanneal ./charts/autoanneal \
  --set secrets.anthropicApiKey=$ANTHROPIC_API_KEY \
  --set secrets.githubToken=$GH_TOKEN \
  --set 'repos[0].name=my-service' \
  --set 'repos[0].repo=myorg/my-service' \
  --set 'repos[0].schedule=0 3 * * 1'
```

See [docs/kubernetes.md](docs/kubernetes.md) for the full values.yaml reference, production patterns, and examples.

## CLI reference

```
autoanneal <repo-url> [OPTIONS]
```

`<repo-url>` accepts `owner/repo` or a full GitHub URL.

| Flag | Default | Description |
|------|---------|-------------|
| `--max-budget <USD>` | `5.00` | Total Claude spend cap |
| `--timeout <duration>` | `30m` | Wall-clock timeout for the entire run |
| `--model <model>` | `sonnet` | Claude model alias or ID |
| `--max-tasks <N>` | `5` | Max improvements to implement |
| `--dry-run` | — | Run analysis only, print JSON, skip PR creation |
| `--keep-on-failure` | — | Skip cleanup on failure (for debugging) |
| `--setup-command <cmd>` | — | Shell command run after clone (e.g. `npm install`) |
| `--min-severity <level>` | `minor` | Filter threshold: `minor`, `moderate`, `major` |
| `--log-level <level>` | `info` | `off`, `error`, `warn`, `info`, `debug`, `trace` |
| `--output <format>` | `text` | Output format: `text` or `json` |
| `--skip-after <N>` | `3` | Skip analysis if no commits in N×cron-interval; set 0 to disable |
| `--cron-interval <mins>` | `10` | Cron interval in minutes (used with --skip-after) |
| `--fix-ci` | `true` | Fix PRs with failing CI before new improvements |
| `--fix-conflicts` | `true` | Rebase PRs with merge conflicts before new improvements |
| `--critic-threshold <score>` | `6` | Min critic score (1–10) to create a PR; set 0 to disable |
| `--ci-retries <N>` | `3` | Max CI fix attempts |
| `--improve-docs` | `true` | Fall back to doc improvements when no code improvements found |
| `--doc-critic-threshold <score>` | `7` | Min critic score for documentation changes |
| `--review-prs` | `false` | Review external PRs (not created by autoanneal) |
| `--review-filter <filter>` | `all` | PR review filter: `all`, `labeled:<label>`, or `recent` |
| `--review-fix-threshold <score>` | `7` | If critic score below this, attempt fixes instead of just commenting |
| `--concurrency <N>` | `3` | Maximum concurrent work items |
| `--max-open-prs <N>` | `5` | Max open autoanneal PRs before skipping; 0 = unlimited |
| `--investigate-issues <labels>` | `""` | Investigate issues with these labels (comma-separated); empty = disabled |
| `--max-issues <N>` | `2` | Max issues to investigate per run |
| `--issue-budget <USD>` | `3.00` | Budget per issue investigation (USD) |

## How it works

The tool runs five sequential phases:

1. **Preflight + Recon** — Validates tokens, clones the repo, detects the tech stack, and asks Claude to produce an architecture summary.
2. **Analysis** — Claude explores the codebase with read-only tools and returns a ranked list of improvements with severity, risk, and scope estimates.
3. **Plan + PR** — Creates a branch, generates a PR description from the improvement list, and opens a draft PR.
4. **Implement** — Iterates through each improvement: Claude makes the changes, guardrails validate scope (file allowlist, line count caps), a build check runs, and the result is committed and pushed.
5. **Critic + CI** — Independent review scoring and CI failure auto-fix.

Each phase has its own budget allocation and timeout. High-risk or oversized changes are automatically skipped.

See [docs/architecture.md](docs/architecture.md) for the full phase pipeline, budget allocation, and guardrail details.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | All or some tasks succeeded (PR URL printed), or analysis found nothing to do |
| `1` | No tasks succeeded, or pre-flight failure |
| `2` | Budget or timeout exhausted mid-run (partial work committed as draft) |

## Documentation

- [docs/architecture.md](docs/architecture.md) — Phase pipeline, data flow, guardrails, cleanup behavior
- [docs/prompts.md](docs/prompts.md) — Prompt strategy, JSON schemas, customization
- [docs/docker.md](docs/docker.md) — Image structure, recommended run flags, extending the image
- [docs/kubernetes.md](docs/kubernetes.md) — Helm chart installation, values.yaml reference, examples

## License

TBD
