use crate::models::Severity;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "autoanneal", about = "Autonomous code improvement agent")]
pub struct Config {
    /// GitHub repository (owner/repo or full URL)
    pub repo: String,

    /// Total Claude spend cap in USD
    #[arg(long, default_value = "5.00")]
    pub max_budget: f64,

    /// Total wall-clock timeout (e.g., "30m", "1h")
    #[arg(long, default_value = "30m")]
    pub timeout: String,

    /// Default Claude model alias or ID (used for phases without a specific override)
    #[arg(long, default_value = "sonnet")]
    pub model: String,

    /// Model for recon phase (defaults to --model)
    #[arg(long)]
    pub model_recon: Option<String>,

    /// Model for analysis phase (defaults to --model)
    #[arg(long)]
    pub model_analysis: Option<String>,

    /// Model for implementation phase (defaults to --model)
    #[arg(long)]
    pub model_implement: Option<String>,

    /// Model for critic review phase (defaults to --model)
    #[arg(long)]
    pub model_critic: Option<String>,

    /// Model for plan/PR body generation (defaults to --model)
    #[arg(long)]
    pub model_plan: Option<String>,

    /// Maximum number of improvements to implement
    #[arg(long, default_value = "5")]
    pub max_tasks: usize,

    /// Run analysis only, print JSON, no PR
    #[arg(long)]
    pub dry_run: bool,

    /// Skip cleanup on failure (for debugging)
    #[arg(long)]
    pub keep_on_failure: bool,

    /// Shell command to run after clone
    #[arg(long)]
    pub setup_command: Option<String>,

    /// Minimum improvement severity to include
    #[arg(long, default_value = "minor")]
    pub min_severity: String,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Output format (text or json)
    #[arg(long, default_value = "text")]
    pub output: String,

    /// Skip analysis if no commits anywhere (including autoanneal/ branches)
    /// are newer than this many multiples of the cron interval.
    /// E.g., with a 10m cron and skip_after=3, skip if nothing changed in 30m.
    /// Set to 0 to disable skip logic.
    #[arg(long, default_value = "3")]
    pub skip_after: usize,

    /// Cron interval in minutes (used with skip_after to calculate staleness).
    #[arg(long, default_value = "10")]
    pub cron_interval: u64,

    /// Fix PRs with failing CI before looking for new improvements.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub fix_ci: bool,

    /// Rebase PRs with merge conflicts before looking for new improvements.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub fix_conflicts: bool,

    /// Minimum critic score (1-10) to create a PR. Set to 0 to disable critic.
    #[arg(long, default_value = "6")]
    pub critic_threshold: u32,

    /// Fall back to documentation improvements when no code improvements found.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub improve_docs: bool,

    /// Minimum critic score for documentation changes (higher bar than code).
    #[arg(long, default_value = "7")]
    pub doc_critic_threshold: u32,

    /// Review external PRs (not created by autoanneal).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub review_prs: bool,

    /// Only review PRs matching this filter: "all", "labeled:<label>", or "recent" (updated in last 24h).
    #[arg(long, default_value = "all")]
    pub review_filter: String,

    /// If critic score is below this threshold, attempt to fix issues instead of just commenting.
    #[arg(long, default_value = "7")]
    pub review_fix_threshold: u32,

    /// Maximum concurrent work items.
    #[arg(long, default_value = "3")]
    pub concurrency: usize,

    /// Maximum open autoanneal PRs before skipping new analysis. 0 = unlimited.
    #[arg(long, default_value = "5")]
    pub max_open_prs: usize,

    /// Investigate open GitHub issues with this label (comma-separated). Empty = disabled.
    #[arg(long, default_value = "")]
    pub investigate_issues: String,

    /// Maximum issues to investigate per run.
    #[arg(long, default_value = "2")]
    pub max_issues: usize,

    /// Budget per issue investigation (USD).
    #[arg(long, default_value = "3.00")]
    pub issue_budget: f64,
}

impl Config {
    /// Get the model for a specific phase, falling back to the default.
    pub fn model_for(&self, phase: &str) -> &str {
        match phase {
            "recon" => self.model_recon.as_deref().unwrap_or(&self.model),
            "analysis" => self.model_analysis.as_deref().unwrap_or(&self.model),
            "implement" => self.model_implement.as_deref().unwrap_or(&self.model),
            "critic" => self.model_critic.as_deref().unwrap_or(&self.model),
            "plan" => self.model_plan.as_deref().unwrap_or(&self.model),
            _ => &self.model,
        }
    }

    /// Parse the repo string into "owner/repo" format.
    /// Handles both "owner/repo" and "https://github.com/owner/repo" formats.
    pub fn repo_slug(&self) -> String {
        let s = &self.repo;

        let slug = if let Some(rest) = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("http://github.com/"))
            .or_else(|| s.strip_prefix("github.com/"))
            .or_else(|| {
                // Handle SSH URL format: git@github.com:owner/repo.git
                if let Some(at_pos) = s.find('@') {
                    let after_at = &s[at_pos + 1..];
                    after_at.find(':').map(|colon_pos| &after_at[colon_pos + 1..])
                } else {
                    None
                }
            })
        {
            rest.to_string()
        } else {
            s.clone()
        };

        let slug = slug.strip_suffix(".git").unwrap_or(&slug).to_string();

        // Trim trailing slash
        slug.trim_end_matches('/').to_string()
    }

    /// Parse the timeout string into a Duration.
    /// Supports strings like "30m", "1h", "1h30m", "90s".
    /// Falls back to 30 minutes for empty strings (silent) or invalid strings (with warning).
    pub fn timeout_duration(&self) -> std::time::Duration {
        match parse_duration(&self.timeout) {
            Ok(duration) => duration,
            Err(DurationError::Empty) => std::time::Duration::from_secs(30 * 60),
            Err(DurationError::Invalid) => {
                eprintln!(
                    "Warning: Invalid timeout string '{}', using default 30m",
                    self.timeout
                );
                std::time::Duration::from_secs(30 * 60)
            }
        }
    }

    /// Parse min_severity string into Severity enum.
    pub fn min_severity(&self) -> Severity {
        match self.min_severity.to_lowercase().as_str() {
            "minor" => Severity::Minor,
            "moderate" => Severity::Moderate,
            "major" => Severity::Major,
            _ => Severity::Minor,
        }
    }
}

/// Error type for duration parsing failures.
#[derive(Debug, PartialEq)]
pub enum DurationError {
    /// Input was empty or whitespace-only.
    Empty,
    /// Input was invalid or unparseable.
    Invalid,
}

/// Parse a duration string like "30m", "1h", "1h30m", "90s" into a Duration.
/// Supports `h`, `m`, `s` suffixes.
/// Returns Err(DurationError::Empty) for empty strings,
/// or Err(DurationError::Invalid) for unparseable strings.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, DurationError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(DurationError::Empty);
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            let n: u64 = current_num.parse().map_err(|_| DurationError::Invalid)?;
            current_num.clear();

            match c {
                'h' | 'H' => total_secs += n * 3600,
                'm' | 'M' => total_secs += n * 60,
                's' | 'S' => total_secs += n,
                _ => return Err(DurationError::Invalid),
            }
        }
    }

    // Handle bare number (no suffix) — treat as seconds
    if !current_num.is_empty() {
        let n: u64 = current_num.parse().map_err(|_| DurationError::Invalid)?;
        total_secs += n;
    }

    if total_secs == 0 {
        return Err(DurationError::Invalid);
    }

    Ok(std::time::Duration::from_secs(total_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("30m"), Ok(std::time::Duration::from_secs(1800)));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h"), Ok(std::time::Duration::from_secs(3600)));
    }

    #[test]
    fn test_parse_duration_combined() {
        assert_eq!(parse_duration("1h30m"), Ok(std::time::Duration::from_secs(5400)));
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("90s"), Ok(std::time::Duration::from_secs(90)));
    }

    #[test]
    fn test_parse_duration_bare_number() {
        assert_eq!(parse_duration("120"), Ok(std::time::Duration::from_secs(120)));
    }

    #[test]
    fn test_parse_duration_empty() {
        assert_eq!(parse_duration(""), Err(DurationError::Empty));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert_eq!(parse_duration("30x"), Err(DurationError::Invalid));
        assert_eq!(parse_duration("abc"), Err(DurationError::Invalid));
        assert_eq!(parse_duration("1h2x3m"), Err(DurationError::Invalid));
    }

    #[test]
    fn test_repo_slug_owner_repo() {
        let config = Config {
            repo: "owner/repo".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_full_url() {
        let config = Config {
            repo: "https://github.com/owner/repo".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_with_git_suffix() {
        let config = Config {
            repo: "https://github.com/owner/repo.git".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_ssh_url() {
        let config = Config {
            repo: "git@github.com:owner/repo.git".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_min_severity_parsing() {
        let config = Config {
            repo: "owner/repo".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "moderate".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.min_severity(), Severity::Moderate);
    }

    #[test]
    fn test_min_severity_unknown_defaults_to_minor() {
        let config = Config {
            repo: "owner/repo".to_string(),
            max_budget: 5.0,
            timeout: "30m".to_string(),
            model: "sonnet".to_string(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "unknown".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
            skip_after: 3,
            cron_interval: 10,
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
        };
        assert_eq!(config.min_severity(), Severity::Minor);
    }
}
