use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Repository metadata from `gh repo view --json ...`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub disk_usage_kb: u64,
    pub viewer_permission: String,
}

/// Detected project stack from file scanning (package.json, Cargo.toml, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackInfo {
    pub primary_language: String,
    pub build_commands: Vec<String>,
    pub test_commands: Vec<String>,
    pub lint_commands: Vec<String>,
    pub key_directories: Vec<String>,
    pub has_ci: bool,
    pub ci_files: Vec<String>,
}

/// An open pull request from `gh pr list --json ...`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub head_ref: String,
    pub files: Vec<String>,
}

/// Claude's structured output from the recon phase.
/// Matches the ReconSchema JSON schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconResult {
    pub summary: String,
    pub primary_language: String,
    pub build_commands: Vec<String>,
    pub test_commands: Vec<String>,
    #[serde(default)]
    pub lint_commands: Vec<String>,
    #[serde(default)]
    pub key_directories: Vec<String>,
}

/// Severity level for an improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Minor,
    Moderate,
    Major,
}

impl<'de> serde::Deserialize<'de> for Severity {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?.to_lowercase();
        match s.as_str() {
            "minor" | "low" => Ok(Severity::Minor),
            "moderate" | "medium" => Ok(Severity::Moderate),
            "major" | "high" | "critical" => Ok(Severity::Major),
            _ => Ok(Severity::Minor), // default to minor for unknown values
        }
    }
}

/// Category of an improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Category {
    Bug,
    Performance,
    Security,
    Quality,
    Testing,
    Docs,
    ErrorHandling,
}

impl<'de> serde::Deserialize<'de> for Category {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?.to_lowercase().replace('-', "_");
        match s.as_str() {
            "bug" | "bug_fix" | "bugfix" => Ok(Category::Bug),
            "performance" | "perf" => Ok(Category::Performance),
            "security" => Ok(Category::Security),
            "quality" | "code_quality" | "refactor" => Ok(Category::Quality),
            "testing" | "test" | "tests" => Ok(Category::Testing),
            "docs" | "documentation" => Ok(Category::Docs),
            "error_handling" | "error handling" | "error" => Ok(Category::ErrorHandling),
            _ => Ok(Category::Quality), // default
        }
    }
}

/// Risk level for an improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl<'de> serde::Deserialize<'de> for Risk {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?.to_lowercase();
        match s.as_str() {
            "low" | "minor" => Ok(Risk::Low),
            "medium" | "moderate" => Ok(Risk::Medium),
            "high" | "major" | "critical" => Ok(Risk::High),
            _ => Ok(Risk::Low), // default
        }
    }
}

/// A single improvement identified during the analysis phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Improvement {
    pub title: String,
    pub description: String,
    pub severity: Severity,
    pub category: Category,
    pub files_to_modify: Vec<String>,
    pub estimated_lines_changed: u32,
    pub risk: Risk,
}

/// Wrapper for the analysis phase structured output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub improvements: Vec<Improvement>,
}

/// Claude's structured output for PR title and body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrBody {
    pub title: String,
    pub body: String,
}

/// CI status of a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CiStatus {
    Passing,
    Failing,
    Pending,
    Fixing,
}

/// An in-flight autoanneal PR detected during preflight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightPr {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub branch: String,
    pub ci_status: CiStatus,
    pub has_fixing_label: bool,
    pub has_merge_conflicts: bool,
    pub files: Vec<String>,
}

/// Status of a single implementation task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "lowercase")]
pub enum TaskStatus {
    Success,
    Skipped(String),
    Failed(String),
}

/// Outcome of a single implementation task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub title: String,
    pub status: TaskStatus,
    pub cost_usd: f64,
    pub files_changed: Vec<String>,
}

/// Result of validating a git diff against guardrail constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    pub files_changed: Vec<String>,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub extra_files: Vec<String>,
}


/// Result of a critic review of code changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticResult {
    pub score: u32,
    pub verdict: String,
    pub summary: String,
}

/// An external (non-autoanneal) PR detected during preflight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalPr {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub author: String,
    pub updated_at: String,
    pub labels: Vec<String>,
}

/// A GitHub issue fetched for investigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// Summary of a single phase for the final report.
#[derive(Debug, Clone)]
pub struct PhaseReport {
    pub name: String,
    pub duration: Duration,
    pub cost_usd: f64,
    pub status: String,
}

/// Gate 1 (WORTHWHILE) critic response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorthwhileResponse {
    /// "worthwhile" | "needs_work" | "reject"
    pub verdict: String,
    pub confidence: f64,
    pub reasoning: String,
}

/// Gate 2 (READY) critic response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyResponse {
    pub verdict: String,
    pub issues: Vec<CriticIssue>,
    pub reasoning: String,
}

/// An issue identified by a critic in Gate 2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticIssue {
    pub file: String,
    pub description: String,
    pub severity: String,
    #[serde(default)]
    pub suggested_fix: Option<String>,
}

/// Gate 3 (VERDICT) critic response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictResponse {
    pub score: u32,
    pub summary: String,
}

/// Result of a single gate's execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: String,
    pub passed: bool,
    pub round1_responses: Vec<CriticEntry>,
    #[serde(default)]
    pub round2_responses: Vec<CriticEntry>,
    #[serde(default)]
    pub research_findings: Option<String>,
}

/// A single critic's response within a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticEntry {
    pub model: String,
    pub role_hint: String,
    pub cost_usd: f64,
}

/// Full result of the 3-gate deliberation pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliberationResult {
    pub approved: bool,
    pub score: u32,
    pub summary: String,
    pub cost_usd: f64,
    pub made_fixes: bool,
    pub gate_results: Vec<GateResult>,
}
