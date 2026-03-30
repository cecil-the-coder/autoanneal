use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// How often to check rate limits (every N gh commands).
const RATE_CHECK_INTERVAL: u64 = 10;

/// Start throttling when remaining requests drop below this.
const RATE_LIMIT_LOW_THRESHOLD: u64 = 200;

/// Sleep this long between calls when throttling.
const THROTTLE_DELAY: Duration = Duration::from_secs(2);

/// Global counter of gh commands since last rate limit check.
static CALL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cached remaining rate limit (updated every RATE_CHECK_INTERVAL calls).
/// Initialized to 0 to force a rate limit check on first call; if the check fails,
/// the exhaustion logic will wait rather than allowing unthrottled calls.
static RATE_REMAINING: AtomicU64 = AtomicU64::new(0);

/// Cached rate limit reset timestamp (unix seconds).
static RATE_RESET: AtomicU64 = AtomicU64::new(0);

/// Run a gh CLI command with retry, exponential backoff, and rate limit awareness.
///
/// Retries on 5xx errors and rate limits (up to 3 attempts).
/// Fails immediately on 401 (auth failure).
/// Periodically checks GitHub rate limits and throttles if running low.
pub async fn gh_command(repo_dir: &Path, args: &[&str]) -> Result<String> {
    // Proactive rate limit check every N calls.
    let count = CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    if count % RATE_CHECK_INTERVAL == 0 {
        check_rate_limit().await;
    }

    // If rate limit is low, add a delay.
    let remaining = RATE_REMAINING.load(Ordering::Relaxed);
    if remaining < RATE_LIMIT_LOW_THRESHOLD && remaining > 0 {
        tracing::warn!(
            remaining,
            "GitHub rate limit is low, throttling"
        );
        tokio::time::sleep(THROTTLE_DELAY).await;
    }

    // If rate limit is exhausted, wait until reset.
    if remaining == 0 {
        let reset = RATE_RESET.load(Ordering::Relaxed);
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => {
                let now = d.as_secs();
                if reset > now {
                    let wait = reset.saturating_sub(now).saturating_add(1);
                    tracing::warn!(
                        wait_secs = wait,
                        "GitHub rate limit exhausted, waiting for reset"
                    );
                    tokio::time::sleep(Duration::from_secs(wait.min(300))).await;
                }
            }
            Err(_) => {
                tracing::warn!("system clock is before UNIX epoch, skipping rate limit wait");
            }
        }
    }

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
                .map_err(|e| anyhow::anyhow!("gh command produced invalid UTF-8: {e}"))?;
            return Ok(stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_lowercase();

        // Auth failures are not transient -- bail immediately.
        if stderr_lower.contains("401") || stderr_lower.contains("authentication") {
            bail!("gh authentication failure: {stderr}");
        }

        // "no checks reported" is permanent, not transient -- bail immediately.
        if stderr_lower.contains("no checks reported") {
            bail!("gh command failed: {stderr}");
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

        // On rate limit errors, force a fresh check.
        if stderr_lower.contains("rate limit") {
            check_rate_limit().await;
        }

        if attempt < MAX_ATTEMPTS {
            tracing::warn!(
                attempt,
                max_attempts = MAX_ATTEMPTS,
                backoff_secs = backoff.as_secs(),
                stderr = %stderr,
                "gh command failed with transient error, retrying",
            );
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        } else {
            bail!(
                "gh command failed after {MAX_ATTEMPTS} attempts: {stderr}"
            );
        }
    }

    bail!("gh command failed: all {MAX_ATTEMPTS} retry attempts exhausted");
}

/// Run a gh CLI command that returns JSON, then parse it into `T`.
pub async fn gh_json<T: serde::de::DeserializeOwned>(
    repo_dir: &Path,
    args: &[&str],
) -> Result<T> {
    let raw = gh_command(repo_dir, args).await?;
    serde_json::from_str(&raw).context("failed to parse gh JSON output")
}

/// Check the current GitHub API rate limit and update cached values.
async fn check_rate_limit() {
    let dot = Path::new(".");
    let command_future = tokio::process::Command::new("gh")
        .args(["api", "rate_limit", "--jq", ".rate | \"\\(.remaining) \\(.reset)\""])
        .current_dir(dot)
        .output();

    let output = tokio::time::timeout(Duration::from_secs(10), command_future).await;

    let output = match output {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("Rate limit check timed out after 10 seconds");
            return;
        }
    };

    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<&str> = s.trim().split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(remaining) = parts[0].parse::<u64>() {
                    RATE_REMAINING.store(remaining, Ordering::Relaxed);
                }
                if let Ok(reset) = parts[1].parse::<u64>() {
                    RATE_RESET.store(reset, Ordering::Relaxed);
                }
                tracing::debug!(
                    remaining = parts[0],
                    reset = parts[1],
                    "GitHub rate limit check"
                );
            }
        }
    }
}
