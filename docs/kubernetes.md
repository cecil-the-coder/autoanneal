# Kubernetes Deployment

Deploy autoanneal as scheduled CronJobs using the included Helm chart. Each repo in your configuration gets its own CronJob with independent schedule and settings.

## Prerequisites

- Helm 3
- kubectl configured for your target cluster
- `ANTHROPIC_API_KEY` and a GitHub token with `repo` scope

## Installation

From the repo root:

```bash
helm install autoanneal ./charts/autoanneal \
  --set secrets.anthropicApiKey=$ANTHROPIC_API_KEY \
  --set secrets.githubToken=$GH_TOKEN \
  --set 'repos[0].name=my-service' \
  --set 'repos[0].repo=myorg/my-service' \
  --set 'repos[0].schedule=0 3 * * 1'
```

Or with a custom values file:

```bash
helm install autoanneal ./charts/autoanneal -f my-values.yaml
```

## values.yaml reference

### `image`

Container image settings.

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` | `ghcr.io/cecil-the-coder/autoanneal` | Image registry and name |
| `image.tag` | `latest` | Image tag |
| `image.pullPolicy` | `IfNotPresent` | Kubernetes pull policy |
| `imagePullSecrets` | `[]` | Registry pull secrets |

### `secrets`

API keys and tokens. The chart creates a Kubernetes Secret unless `existingSecret` is set.

| Key | Default | Description |
|-----|---------|-------------|
| `secrets.existingSecret` | `""` | Name of a pre-existing Secret (skips creation) |
| `secrets.anthropicApiKey` | `""` | Anthropic API key (stored in chart-managed Secret) |
| `secrets.githubToken` | `""` | GitHub token (stored in chart-managed Secret) |

The Secret must contain keys `ANTHROPIC_API_KEY` and `GH_TOKEN`.

### `defaults`

Global defaults applied to all repos. Individual repos can override any of these.

| Key | Default | Description |
|-----|---------|-------------|
| `defaults.maxBudget` | `"5.00"` | Claude spend cap (USD) |
| `defaults.timeout` | `"30m"` | Wall-clock timeout |
| `defaults.model` | `"sonnet"` | Claude model alias or ID |
| `defaults.maxTasks` | `5` | Max improvements per run |
| `defaults.minSeverity` | `"minor"` | Severity filter: `minor`, `moderate`, `major` |
| `defaults.logLevel` | `"info"` | Log level |
| `defaults.dryRun` | `false` | Analysis only, no PR |
| `defaults.setupCommand` | `""` | Shell command run after clone |
| `defaults.skipAfter` | `3` | Skip repo if no commits in `skipAfter` × `cronInterval` minutes |
| `defaults.cronInterval` | `10` | Cron interval in minutes (used for staleness calculation) |
| `defaults.fixCi` | `true` | Fix PRs with failing CI before looking for new improvements |
| `defaults.fixConflicts` | `true` | Rebase PRs with merge conflicts |
| `defaults.criticThreshold` | `6` | Minimum critic score (1–10) to create a PR. `0` disables the critic. |
| `defaults.improveDocs` | `true` | Fall back to documentation improvements when no code improvements found |
| `defaults.docCriticThreshold` | `7` | Minimum critic score for documentation changes |
| `defaults.reviewPrs` | `false` | Review external PRs (not created by autoanneal) |
| `defaults.reviewFilter` | `"all"` | Filter for external PRs: `"all"`, `"labeled:<label>"`, or `"recent"` |
| `defaults.reviewFixThreshold` | `7` | If critic score is below this, attempt to fix instead of just commenting |
| `defaults.concurrency` | `3` | Maximum concurrent work items |
| `defaults.maxOpenPrs` | `5` | Skip new analysis if this many autoanneal PRs are already open |
| `defaults.investigateIssues` | `""` | Investigate open GitHub issues with this label (comma-separated). Empty = disabled. |
| `defaults.maxIssues` | `2` | Maximum issues to investigate per run |
| `defaults.issueBudget` | `"3.00"` | Budget per issue investigation (USD) |

### `env`

Additional environment variables injected into all containers. Useful for model overrides or debug flags.

```yaml
env:
  ANTHROPIC_BASE_URL: "https://custom-proxy.example.com"
  AUTOANNEAL_DEBUG_STREAM: "1"
```

### `repos`

List of repositories to target. Each entry creates a separate CronJob (or Job if `job.enabled` is true).

```yaml
repos:
  - name: my-service            # DNS-safe name, used in resource names
    repo: "myorg/my-service"    # owner/repo
    schedule: "0 3 * * 1"       # Cron schedule (UTC)
    # Optional per-repo overrides:
    maxBudget: "10.00"
    timeout: "1h"
    model: "opus"
    maxTasks: 3
    minSeverity: "moderate"
    setupCommand: "npm ci"
```

The `name` field must be DNS-safe (lowercase, alphanumeric, hyphens only).

### `job`

One-off Job mode. Creates Jobs instead of CronJobs.

| Key | Default | Description |
|-----|---------|-------------|
| `job.enabled` | `false` | Create Jobs instead of CronJobs |
| `job.repoFilter` | `""` | Run only this repo (by name). Empty = all repos. |

### `cronJob`

CronJob behavior settings.

| Key | Default | Description |
|-----|---------|-------------|
| `cronJob.concurrencyPolicy` | `Forbid` | Prevent overlapping runs |
| `cronJob.failedJobsHistoryLimit` | `3` | Failed job history to retain |
| `cronJob.successfulJobsHistoryLimit` | `3` | Successful job history to retain |
| `cronJob.startingDeadlineSeconds` | `600` | Deadline for missed schedules |
| `cronJob.suspend` | `false` | Suspend all CronJobs |
| `cronJob.backoffLimit` | `0` | Pod restart attempts (0 = no retry) |
| `cronJob.activeDeadlineSeconds` | `3600` | Hard timeout for the Job (seconds) |
| `cronJob.ttlSecondsAfterFinished` | `86400` | Auto-cleanup completed Jobs after 24h |

### `resources`

```yaml
resources:
  requests:
    cpu: "1"
    memory: "2Gi"
  limits:
    cpu: "2"
    memory: "4Gi"
```

### Pod settings

`podAnnotations`, `podLabels`, `nodeSelector`, `tolerations`, `affinity`, `podSecurityContext`, `containerSecurityContext`, and `serviceAccount` are all available. See `values.yaml` for defaults.

## Examples

### Single repo, weekly CronJob

```yaml
secrets:
  existingSecret: autoanneal-secrets

repos:
  - name: api-server
    repo: "myorg/api-server"
    schedule: "0 3 * * 1"
```

### Multiple repos with different schedules

```yaml
defaults:
  maxBudget: "8.00"
  model: "sonnet"

repos:
  - name: api-server
    repo: "myorg/api-server"
    schedule: "0 3 * * 1"         # Monday 3am
    maxTasks: 5

  - name: web-frontend
    repo: "myorg/web-frontend"
    schedule: "0 4 * * 3"         # Wednesday 4am
    setupCommand: "npm ci"
    maxBudget: "10.00"

  - name: ml-pipeline
    repo: "myorg/ml-pipeline"
    schedule: "0 2 1 * *"         # First of month
    model: "opus"
    timeout: "1h"
    minSeverity: "moderate"
```

### One-off Job run

Run all repos immediately as Jobs instead of waiting for the cron schedule:

```bash
helm install autoanneal-run ./charts/autoanneal \
  -f my-values.yaml \
  --set job.enabled=true
```

Run a single repo:

```bash
helm install autoanneal-run ./charts/autoanneal \
  -f my-values.yaml \
  --set job.enabled=true \
  --set job.repoFilter=api-server
```

Clean up after the run:

```bash
helm uninstall autoanneal-run
```

### Using an existing Kubernetes Secret (production)

Create the Secret separately (via a secrets manager, sealed-secrets, external-secrets, etc.):

```bash
kubectl create secret generic autoanneal-secrets \
  --from-literal=ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
  --from-literal=GH_TOKEN="$GH_TOKEN"
```

Reference it in values:

```yaml
secrets:
  existingSecret: autoanneal-secrets
```

This avoids passing secrets via `--set` or storing them in values files.

### Triggering a CronJob manually

```bash
kubectl create job --from=cronjob/autoanneal-api-server api-server-manual
```

The job name (`autoanneal-api-server`) follows the pattern `<release>-<repo-name>`.

## Monitoring

### Check CronJob status

```bash
kubectl get cronjobs -l app.kubernetes.io/instance=autoanneal
```

### View recent Jobs

```bash
kubectl get jobs -l app.kubernetes.io/instance=autoanneal --sort-by=.metadata.creationTimestamp
```

### View logs from the latest run

```bash
kubectl logs job/autoanneal-api-server-<id>
```

Or find the pod directly:

```bash
kubectl get pods -l app.kubernetes.io/instance=autoanneal --sort-by=.metadata.creationTimestamp
kubectl logs <pod-name>
```

### Suspend all CronJobs

```bash
helm upgrade autoanneal ./charts/autoanneal -f my-values.yaml --set cronJob.suspend=true
```

## Uninstall

```bash
helm uninstall autoanneal
```

This removes all CronJobs, Jobs, and the chart-managed Secret. It does not delete Secrets created externally via `existingSecret`.
