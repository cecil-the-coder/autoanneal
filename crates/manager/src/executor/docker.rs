use async_trait::async_trait;
use anyhow::{Context, Result};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::{Executor, PendingRun, RunOutcome};
use autoanneal_lib::result::{WorkerResult, RESULT_MARKER};

pub struct DockerExecutor;

#[async_trait]
impl Executor for DockerExecutor {
    async fn launch(&self, run: &PendingRun) -> Result<()> {
        let container_name = format!("autoanneal-{}", run.run_id);
        let args = run.repo_entry.to_worker_args(&run.defaults);

        let mut cmd = Command::new("docker");
        cmd.args(["run", "--name", &container_name]);

        // Pass through env vars for secrets
        for var in &["ANTHROPIC_API_KEY", "GH_TOKEN", "ANTHROPIC_BASE_URL",
                     "ANTHROPIC_AUTH_TOKEN", "OPENAI_BASE_URL", "OPENAI_API_KEY",
                     "AUTOANNEAL_PROVIDER"] {
            if std::env::var(var).is_ok() {
                cmd.arg("--env").arg(format!("{var}=${{{var}}}"));
            }
        }

        // Set result path env var
        cmd.arg("--env").arg(format!("AUTOANNEAL_RESULT_PATH={}", run.result_path));

        cmd.arg(&run.worker_image);
        cmd.args(&args);

        debug!(container = %container_name, "launching docker container");

        let output = cmd.output().await
            .context("failed to run docker")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Container might have exited with non-zero but that's okay -
            // we'll collect results later. Only error if docker itself failed.
            if stderr.contains("No such image") || stderr.contains("permission denied") {
                anyhow::bail!("docker run failed: {stderr}");
            }
        }

        info!(container = %container_name, repo = %run.repo_entry.repo, "docker container launched");
        Ok(())
    }

    async fn is_running(&self, _repo_name: &str, run_id: &str) -> Result<bool> {
        let container_name = format!("autoanneal-{run_id}");
        let output = Command::new("docker")
            .args(["inspect", &container_name, "--format", "{{.State.Running}}"])
            .output().await
            .context("failed to inspect container")?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(stdout == "true")
    }

    async fn collect(&self, repo_name: &str, run_id: &str) -> Result<Option<RunOutcome>> {
        let container_name = format!("autoanneal-{run_id}");

        // Check if still running
        if self.is_running(repo_name, run_id).await? {
            return Ok(None);
        }

        // Get exit code
        let exit_output = Command::new("docker")
            .args(["inspect", &container_name, "--format", "{{.State.ExitCode}}"])
            .output().await?;

        let exit_code: i32 = String::from_utf8_lossy(&exit_output.stdout)
            .trim().parse().unwrap_or(-1);

        // Get logs and extract result
        let logs = Command::new("docker")
            .args(["logs", &container_name])
            .output().await?;

        let log_str = String::from_utf8_lossy(&logs.stdout);
        let result = parse_result_from_logs(&log_str);

        // Get start/finish times for duration
        let _start_output = Command::new("docker")
            .args(["inspect", &container_name, "--format", "{{.State.StartedAt}}"])
            .output().await?;

        let duration = Duration::from_secs(0); // Will be filled from WorkerResult if available

        // Cleanup container
        let _ = Command::new("docker")
            .args(["rm", &container_name])
            .output().await;

        let duration = result.as_ref()
            .map(|r| Duration::from_secs(r.total_duration_secs))
            .unwrap_or(duration);

        Ok(Some(RunOutcome { exit_code, duration, result }))
    }

    async fn cancel(&self, _repo_name: &str, run_id: &str) -> Result<()> {
        let container_name = format!("autoanneal-{run_id}");
        Command::new("docker")
            .args(["stop", &container_name])
            .output().await
            .context("failed to stop container")?;
        let _ = Command::new("docker")
            .args(["rm", &container_name])
            .output().await;
        Ok(())
    }
}

fn parse_result_from_logs(logs: &str) -> Option<WorkerResult> {
    for line in logs.lines().rev() {
        if let Some(json) = line.strip_prefix(RESULT_MARKER) {
            match serde_json::from_str::<WorkerResult>(json) {
                Ok(result) => return Some(result),
                Err(e) => {
                    warn!(error = %e, "failed to parse result from logs");
                }
            }
        }
    }
    None
}
