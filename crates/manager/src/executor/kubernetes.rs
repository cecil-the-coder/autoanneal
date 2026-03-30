use async_trait::async_trait;
use anyhow::{Context, Result};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams};
use kube::Client;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::{Executor, PendingRun, RunOutcome, parse_result_from_logs};

pub struct KubernetesExecutor {
    client: Client,
    namespace: String,
    #[allow(dead_code)]
    worker_image: String,
    resource_cpu_limit: Option<String>,
    resource_memory_limit: Option<String>,
}

impl KubernetesExecutor {
    pub async fn new(
        namespace: &str,
        worker_image: &str,
        resource_cpu_limit: Option<String>,
        resource_memory_limit: Option<String>,
    ) -> Result<Self> {
        let client = Client::try_default()
            .await
            .context("failed to create kube client (not running in cluster?)")?;
        Ok(Self {
            client,
            namespace: namespace.to_string(),
            worker_image: worker_image.to_string(),
            resource_cpu_limit,
            resource_memory_limit,
        })
    }

    /// Build a DNS-safe job name from the run's repo name and run_id.
    fn job_name(run: &PendingRun) -> String {
        let repo_slug: String = run.repo_entry.name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
            .collect();
        let short_hash = &run.run_id[..8.min(run.run_id.len())];
        // K8s job names max 63 chars
        let prefix = format!("autoanneal-{repo_slug}");
        let prefix = &prefix[..52.min(prefix.len())];
        format!("{}-{}", prefix, short_hash)
    }

    fn jobs_api(&self) -> Api<Job> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn pods_api(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }
}

#[async_trait]
impl Executor for KubernetesExecutor {
    async fn launch(&self, run: &PendingRun) -> Result<()> {
        let job_name = Self::job_name(run);
        let args = run.repo_entry.to_worker_args(&run.defaults);

        // Build environment variables from the host process
        let secret_vars = [
            "ANTHROPIC_API_KEY", "GH_TOKEN", "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN", "OPENAI_BASE_URL", "OPENAI_API_KEY",
            "AUTOANNEAL_PROVIDER",
        ];

        let mut env_vars: Vec<serde_json::Value> = Vec::new();
        for var in &secret_vars {
            if let Ok(val) = std::env::var(var) {
                env_vars.push(serde_json::json!({
                    "name": var,
                    "value": val,
                }));
            }
        }

        // Add result path env var
        env_vars.push(serde_json::json!({
            "name": "AUTOANNEAL_RESULT_PATH",
            "value": run.result_path,
        }));

        // Parse timeout for activeDeadlineSeconds
        let timeout_secs = parse_timeout(&run.defaults.timeout);

        // Build resource limits
        let mut limits = serde_json::Map::new();
        if let Some(ref cpu) = self.resource_cpu_limit {
            limits.insert("cpu".into(), serde_json::Value::String(cpu.clone()));
        } else {
            limits.insert("cpu".into(), serde_json::Value::String("2".into()));
        }
        if let Some(ref mem) = self.resource_memory_limit {
            limits.insert("memory".into(), serde_json::Value::String(mem.clone()));
        } else {
            limits.insert("memory".into(), serde_json::Value::String("4Gi".into()));
        }

        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": job_name,
                "labels": {
                    "app": "autoanneal",
                    "autoanneal/repo": run.repo_entry.name,
                    "autoanneal/run-id": run.run_id,
                }
            },
            "spec": {
                "activeDeadlineSeconds": timeout_secs,
                "backoffLimit": 0,
                "template": {
                    "metadata": {
                        "labels": {
                            "app": "autoanneal",
                            "autoanneal/repo": run.repo_entry.name,
                            "autoanneal/run-id": run.run_id,
                        }
                    },
                    "spec": {
                        "restartPolicy": "Never",
                        "containers": [{
                            "name": "worker",
                            "image": run.worker_image,
                            "args": args,
                            "env": env_vars,
                            "resources": {
                                "limits": limits,
                            }
                        }]
                    }
                }
            }
        }))
        .context("failed to build Job spec")?;

        let jobs: Api<Job> = self.jobs_api();
        jobs.create(&PostParams::default(), &job)
            .await
            .context("failed to create Kubernetes Job")?;

        info!(job = %job_name, repo = %run.repo_entry.repo, "kubernetes job created");
        Ok(())
    }

    async fn is_running(&self, _repo_name: &str, run_id: &str) -> Result<bool> {
        let jobs: Api<Job> = self.jobs_api();

        // Find the job by run-id label
        let lp = ListParams::default()
            .labels(&format!("autoanneal/run-id={run_id}"));

        let job_list = jobs.list(&lp).await
            .context("failed to list jobs")?;

        let job = match job_list.items.first() {
            Some(j) => j,
            None => return Ok(false), // Job not found, treat as not running
        };

        // Check if the job has any active pods
        if let Some(ref status) = job.status {
            let active = status.active.unwrap_or(0);
            let succeeded = status.succeeded.unwrap_or(0);
            let failed = status.failed.unwrap_or(0);

            // Still running if active > 0 and not yet completed/failed
            if active > 0 && succeeded == 0 && failed == 0 {
                return Ok(true);
            }
            // If no conditions yet and nothing has succeeded/failed, it may be starting
            if active == 0 && succeeded == 0 && failed == 0 {
                // Check if the job has conditions indicating completion
                if let Some(ref conditions) = status.conditions {
                    for c in conditions {
                        if (c.type_ == "Complete" || c.type_ == "Failed")
                            && c.status == "True"
                        {
                            return Ok(false);
                        }
                    }
                }
                // No completion conditions, might be pending
                return Ok(true);
            }
        } else {
            // No status yet means it's just been created
            return Ok(true);
        }

        Ok(false)
    }

    async fn collect(&self, repo_name: &str, run_id: &str) -> Result<Option<RunOutcome>> {
        // Check if still running
        if self.is_running(repo_name, run_id).await? {
            return Ok(None);
        }

        let jobs: Api<Job> = self.jobs_api();
        let pods: Api<Pod> = self.pods_api();

        // Find the job
        let lp = ListParams::default()
            .labels(&format!("autoanneal/run-id={run_id}"));

        let job_list = jobs.list(&lp).await
            .context("failed to list jobs")?;

        let job = match job_list.items.first() {
            Some(j) => j,
            None => anyhow::bail!("job not found for run_id {run_id}"),
        };

        let job_name = job.metadata.name.as_deref().unwrap_or("unknown");

        // Determine exit code from job status
        let exit_code = if let Some(ref status) = job.status {
            if status.succeeded.unwrap_or(0) > 0 {
                0
            } else {
                1
            }
        } else {
            -1
        };

        // Find the pod(s) for this job
        let pod_lp = ListParams::default()
            .labels(&format!("autoanneal/run-id={run_id}"));

        let pod_list = pods.list(&pod_lp).await
            .context("failed to list pods for job")?;

        let mut log_str = String::new();
        if let Some(pod) = pod_list.items.first() {
            let pod_name = pod.metadata.name.as_deref().unwrap_or("unknown");
            match pods.logs(pod_name, &LogParams::default()).await {
                Ok(logs) => log_str = logs,
                Err(e) => {
                    warn!(pod = %pod_name, error = %e, "failed to read pod logs");
                }
            }

            // Try to get more precise exit code from container status
            if let Some(ref pod_status) = pod.status {
                if let Some(ref container_statuses) = pod_status.container_statuses {
                    if let Some(cs) = container_statuses.first() {
                        if let Some(ref terminated) = cs.state.as_ref().and_then(|s| s.terminated.as_ref()) {
                            let _ = terminated.exit_code; // Use this instead
                            // We'll override exit_code below
                        }
                    }
                }
            }
        }

        // Extract exit code from pod container status (more precise)
        let exit_code = if let Some(pod) = pod_list.items.first() {
            pod.status.as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .and_then(|cs| cs.first())
                .and_then(|cs| cs.state.as_ref())
                .and_then(|s| s.terminated.as_ref())
                .map(|t| t.exit_code)
                .unwrap_or(exit_code)
        } else {
            exit_code
        };

        let result = parse_result_from_logs(&log_str);

        let duration = result.as_ref()
            .map(|r| Duration::from_secs(r.total_duration_secs))
            .unwrap_or_default();

        // Clean up: delete the Job (with propagation to kill pods)
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Background),
            ..Default::default()
        };
        if let Err(e) = jobs.delete(job_name, &dp).await {
            warn!(job = %job_name, error = %e, "failed to delete completed job");
        }

        debug!(job = %job_name, exit_code, "collected job result");

        Ok(Some(RunOutcome {
            exit_code,
            duration,
            result,
        }))
    }

    async fn cancel(&self, _repo_name: &str, run_id: &str) -> Result<()> {
        let jobs: Api<Job> = self.jobs_api();

        let lp = ListParams::default()
            .labels(&format!("autoanneal/run-id={run_id}"));

        let job_list = jobs.list(&lp).await
            .context("failed to list jobs for cancellation")?;

        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Background),
            ..Default::default()
        };

        for job in &job_list.items {
            if let Some(ref name) = job.metadata.name {
                jobs.delete(name, &dp).await
                    .context("failed to delete job")?;
                info!(job = %name, "cancelled kubernetes job");
            }
        }

        Ok(())
    }
}

/// Parse a timeout string like "30m" or "1h" into seconds.
/// Uses checked arithmetic to prevent overflow and caps at 24 hours (86400 seconds).
fn parse_timeout(timeout: &str) -> i64 {
    const MAX_TIMEOUT_SECS: i64 = 24 * 3600; // 24 hours
    const DEFAULT_MINUTES: i64 = 30 * 60;
    const DEFAULT_HOURS: i64 = 1 * 3600;
    const DEFAULT_SECS: i64 = 1800;

    let timeout = timeout.trim();

    // Track which suffix was detected for default value selection
    let (secs, detected_suffix) = if let Some(mins) = timeout.strip_suffix('m') {
        (mins.parse::<i64>().ok().and_then(|m| m.checked_mul(60)), Some('m'))
    } else if let Some(hours) = timeout.strip_suffix('h') {
        (hours.parse::<i64>().ok().and_then(|h| h.checked_mul(3600)), Some('h'))
    } else if let Some(secs) = timeout.strip_suffix('s') {
        (secs.parse::<i64>().ok(), Some('s'))
    } else {
        (timeout.parse::<i64>().ok(), None)
    };

    // Use the parsed value if valid, otherwise use defaults based on detected suffix
    let secs = match secs {
        Some(s) => s,
        None => {
            warn!(timeout = %timeout, "overflow or parse error, using suffix-specific default");
            match detected_suffix {
                Some('m') => DEFAULT_MINUTES,
                Some('h') => DEFAULT_HOURS,
                _ => DEFAULT_SECS,
            }
        }
    };

    // Cap at the maximum to prevent excessive timeouts
    if secs > MAX_TIMEOUT_SECS {
        warn!(timeout = %timeout, value = %secs, max = %MAX_TIMEOUT_SECS, "timeout exceeds maximum, capping");
        MAX_TIMEOUT_SECS
    } else {
        secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timeout_minutes() {
        assert_eq!(parse_timeout("30m"), 30 * 60);
        assert_eq!(parse_timeout("1m"), 60);
        assert_eq!(parse_timeout("0m"), 0);
    }

    #[test]
    fn test_parse_timeout_hours() {
        assert_eq!(parse_timeout("1h"), 3600);
        assert_eq!(parse_timeout("2h"), 7200);
        assert_eq!(parse_timeout("0h"), 0);
    }

    #[test]
    fn test_parse_timeout_seconds() {
        assert_eq!(parse_timeout("90s"), 90);
        assert_eq!(parse_timeout("1s"), 1);
        assert_eq!(parse_timeout("0s"), 0);
    }

    #[test]
    fn test_parse_timeout_bare_number() {
        assert_eq!(parse_timeout("1800"), 1800);
        assert_eq!(parse_timeout("0"), 0);
    }

    #[test]
    fn test_parse_timeout_default_values() {
        // Bare invalid value (no suffix) defaults to 1800 seconds (30 minutes)
        assert_eq!(parse_timeout("abc"), 1800);
        assert_eq!(parse_timeout("10x"), 1800);
        // Invalid hours defaults to 1 hour (3600 seconds)
        assert_eq!(parse_timeout("xh"), 3600);
        // Invalid minutes defaults to 30 minutes (1800 seconds)
        assert_eq!(parse_timeout("xm"), 30 * 60);
        // Invalid seconds defaults to 1800 seconds
        assert_eq!(parse_timeout("xs"), 1800);
    }

    #[test]
    fn test_parse_timeout_overflow() {
        // Very large values should not overflow - use defaults or get capped
        // Minutes value fits in i64 but exceeds 24h, so gets capped
        assert_eq!(parse_timeout("99999999999999999m"), 24 * 3600);
        // Hours multiplier overflows, so uses default (1 hour)
        assert_eq!(parse_timeout("99999999999999999h"), 3600);
        // Seconds value fits in i64 but exceeds 24h, so gets capped  
        assert_eq!(parse_timeout("99999999999999999s"), 24 * 3600);
    }

    #[test]
    fn test_parse_timeout_24h_cap() {
        // Values exceeding 24 hours should be capped
        assert_eq!(parse_timeout("25h"), 24 * 3600);
        assert_eq!(parse_timeout("48h"), 24 * 3600);
        assert_eq!(parse_timeout("1500m"), 24 * 3600);
        assert_eq!(parse_timeout("100000s"), 24 * 3600);
    }

    #[test]
    fn test_parse_timeout_exactly_24h() {
        assert_eq!(parse_timeout("24h"), 24 * 3600);
        assert_eq!(parse_timeout("1440m"), 24 * 3600);
        assert_eq!(parse_timeout("86400s"), 24 * 3600);
    }

    #[test]
    fn test_parse_timeout_whitespace() {
        assert_eq!(parse_timeout("  30m  "), 30 * 60);
        assert_eq!(parse_timeout("  1h  "), 3600);
    }
}

