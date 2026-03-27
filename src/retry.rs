use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::{Duration, SystemTime};

const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_JITTER_MS: u64 = 500;

/// Generate a small random jitter duration (0-500ms) using system time.
fn jitter() -> Duration {
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64)
        .unwrap_or(0)
        % MAX_JITTER_MS;
    Duration::from_millis(millis)
}

/// Run a gh CLI command with retry and exponential backoff.
///
/// Retries on 5xx errors and rate limits (up to 3 attempts).
/// Fails immediately on 401 (auth failure).
pub async fn gh_command(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let mut backoff = INITIAL_BACKOFF;

    for attempt in 1..=MAX_ATTEMPTS {
        let output = tokio::process::Command::new("gh")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .await
            .context("failed to spawn gh process")?;

        if output.status.success() {
            let stdout = String::from_utf8(output.stdout)
                .context("gh output was not valid UTF-8")?;
            return Ok(stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_lowercase();

        // Auth failures are not transient -- bail immediately.
        if stderr_lower.contains("401") || stderr_lower.contains("authentication") {
            bail!("gh authentication failure: {stderr}");
        }

        // Determine whether the error is transient (worth retrying).
        let is_transient = stderr_lower.contains("rate limit")
            || stderr_lower.contains("403")
            || stderr_lower.contains("500")
            || stderr_lower.contains("502")
            || stderr_lower.contains("503")
            || stderr_lower.contains("504");

        if !is_transient {
            bail!("gh command failed: {stderr}");
        }

        if attempt < MAX_ATTEMPTS {
            let jitter = jitter();
            tracing::warn!(
                attempt,
                max_attempts = MAX_ATTEMPTS,
                backoff_secs = backoff.as_secs(),
                jitter_ms = jitter.as_millis(),
                stderr = %stderr,
                "gh command failed with transient error, retrying",
            );
            tokio::time::sleep(backoff + jitter).await;
            backoff *= 2;
        } else {
            bail!(
                "gh command failed after {MAX_ATTEMPTS} attempts: {stderr}"
            );
        }
    }

    unreachable!()
}

/// Run a gh CLI command that returns JSON, then parse it into `T`.
pub async fn gh_json<T: serde::de::DeserializeOwned>(
    repo_dir: &Path,
    args: &[&str],
) -> Result<T> {
    let raw = gh_command(repo_dir, args).await?;
    serde_json::from_str(&raw).context("failed to parse gh JSON output")
}
