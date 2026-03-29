use serde::{Deserialize, Serialize};

/// Schema version for the worker result. Increment when fields change.
pub const RESULT_SCHEMA_VERSION: u32 = 1;

/// Marker prefix emitted to stdout for log scraping by the manager.
pub const RESULT_MARKER: &str = "AUTOANNEAL_RESULT:";

/// Environment variable to optionally write the result to a file.
pub const RESULT_PATH_ENV: &str = "AUTOANNEAL_RESULT_PATH";

/// Structured result produced by a worker run and consumed by the manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Repository slug (owner/repo).
    pub repo: String,
    /// Process exit code (0=success, 1=fatal error, 2=timeout).
    pub exit_code: i32,
    /// Total LLM spend in USD.
    pub total_cost_usd: f64,
    /// Wall-clock duration in seconds.
    pub total_duration_secs: u64,
    /// Per-phase results.
    pub phases: Vec<PhaseResult>,
    /// URL of created PR, if any.
    pub pr_url: Option<String>,
    /// PR number, if a PR was created.
    pub pr_number: Option<u64>,
    /// Branch name used for the run.
    pub branch_name: Option<String>,
    /// Per work-item results.
    pub work_items: Vec<WorkItemResult>,
}

/// Result from a single pipeline phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    pub name: String,
    pub duration_secs: u64,
    pub cost_usd: f64,
    pub status: String,
}

/// Result from a single work item (CI fix, PR review, analysis, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemResult {
    /// "ci_fix", "pr_review", "issue_investigation", "analysis"
    pub kind: String,
    /// Human-readable name (e.g. "CI Fix (PR #42)").
    pub name: String,
    /// "OK", "FAILED", "SKIPPED", "REJECTED (critic)", etc.
    pub status: String,
    pub cost_usd: f64,
    pub duration_secs: u64,
    /// PR URL if the item produced one.
    pub pr_url: Option<String>,
}

impl WorkerResult {
    /// Emit the result to stdout with the marker prefix and optionally to a file.
    pub fn emit(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string(self)?;

        // Always emit to stdout for log scraping.
        println!("{RESULT_MARKER}{json}");

        // Optionally write to file.
        if let Ok(path) = std::env::var(RESULT_PATH_ENV) {
            if !path.is_empty() {
                std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
            }
        }

        Ok(())
    }
}
