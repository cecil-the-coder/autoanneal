use crate::models::Severity;
use clap::Parser;
use tracing::warn;

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
    #[arg(long, default_value = "3")]
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

    /// Comma-separated models for the critic panel (e.g., "sonnet,openai:gpt-4o").
    /// Each model becomes a separate critic instance. Use "provider:model" format
    /// for multi-provider setups. When set, enables the 3-gate deliberation pipeline
    /// instead of the single-critic review.
    #[arg(long)]
    pub critic_models: Option<String>,

    /// Fall back to documentation improvements when no code improvements found.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub improve_docs: bool,

    /// Minimum critic score for documentation changes (higher bar than code).
    #[arg(long, default_value = "7")]
    pub doc_critic_threshold: u32,

    /// Review external PRs (not created by autoanneal).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub review_prs: bool,

    /// Fix CI failures on external PRs (not created by autoanneal).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub fix_external_ci: bool,

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

    /// Context window size in tokens. Old tool results are evicted when the
    /// conversation approaches this limit, and a recall tool lets the model
    /// retrieve them on demand. Lower values reduce cost.
    #[arg(long, default_value = "128000")]
    pub context_window: u64,

    /// Maximum number of fix attempts (CI fix, review fix) on an external PR.
    /// Counted by commits whose message starts with "autoanneal:".
    #[arg(long, default_value = "3")]
    pub max_pr_fix_attempts: u32,

    /// Maximum Exa web searches per run (0 to disable). Requires EXA_API_KEY env var.
    #[arg(long, default_value = "3")]
    pub exa_searches: u32,
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
    ///
    /// SSH URL parsing note: URLs of the form `git@host:owner/repo.git` are
    /// parsed by splitting on the first colon after the `@`. This is correct for
    /// standard GitHub/GitLab SSH URLs, but may produce incorrect results if the
    /// hostname itself contains a colon (e.g., IPv6 literals like `git@[::1]:repo`).
    /// Such edge cases are not expected in normal usage.
    ///
    /// # Panics
    ///
    /// Panics if the resulting slug contains path-traversal sequences (`..`),
    /// which would be a security risk when used to construct file paths.
    pub fn repo_slug(&self) -> String {
        let s = &self.repo;

        let slug = if let Some(rest) = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("http://github.com/"))
            .or_else(|| s.strip_prefix("github.com/"))
            .or_else(|| {
                // Handle SSH URL format: git@github.com:owner/repo.git
                // We find the first '@', then the first ':' after it, which
                // separates the hostname from the path in standard SSH URLs.
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
        let slug = slug.trim_end_matches('/').to_string();

        // Reject slugs containing path-traversal sequences to prevent directory
        // traversal attacks when the slug is used to construct file paths.
        validate_slug(&slug);

        slug
    }

    /// Parse the timeout string into a Duration.
    /// Supports strings like "30m", "1h", "1h30m", "90s".
    ///
    /// If the timeout string is unparseable or overflows, a warning is logged
    /// and the default 30-minute timeout is used instead.
    pub fn timeout_duration(&self) -> std::time::Duration {
        parse_duration(&self.timeout).unwrap_or_else(|| {
            warn!(
                "Failed to parse timeout '{}' (possibly due to overflow); \
                 falling back to default 30m timeout",
                self.timeout
            );
            std::time::Duration::from_secs(30 * 60)
        })
    }

    /// Parse critic_models into a list of model names.
    /// Returns None if critic_models is not set (use single-critic fallback).
    pub fn critic_model_list(&self) -> Option<Vec<String>> {
        let models_str = self.critic_models.as_ref()?;
        if models_str.trim().is_empty() {
            return None;
        }
        let models: Vec<String> = models_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if models.is_empty() {
            None
        } else {
            Some(models)
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

/// Parse a duration string like "30m", "1h", "1h30m", "90s" into a Duration.
/// Supports `h`, `m`, `s` suffixes. Returns None if the string is unparseable.
fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            let n: u64 = current_num.parse().ok()?;
            current_num.clear();

            let secs = match c {
                'h' | 'H' => n.checked_mul(3600)?,
                'm' | 'M' => n.checked_mul(60)?,
                's' | 'S' => n,
                _ => return None,
            };
            total_secs = total_secs.checked_add(secs)?;
        }
    }

    // Handle bare number (no suffix) — treat as seconds
    if !current_num.is_empty() {
        let n: u64 = current_num.parse().ok()?;
        total_secs = total_secs.checked_add(n)?;
    }

    if total_secs == 0 {
        return None;
    }

    Some(std::time::Duration::from_secs(total_secs))
}

/// Validate that a repo slug does not contain path-traversal sequences.
/// Panics if the slug contains `..` as a path component segment, which could
/// be exploited for directory traversal when the slug is used in file paths.
fn validate_slug(slug: &str) {
    if slug.split('/').any(|segment| segment == "..") {
        panic!(
            "repo slug contains path-traversal sequence ('..'): \"{}\" — \
             this is a security risk and is not allowed",
            slug
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("30m"), Some(std::time::Duration::from_secs(1800)));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h"), Some(std::time::Duration::from_secs(3600)));
    }

    #[test]
    fn test_parse_duration_combined() {
        assert_eq!(parse_duration("1h30m"), Some(std::time::Duration::from_secs(5400)));
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("90s"), Some(std::time::Duration::from_secs(90)));
    }

    #[test]
    fn test_parse_duration_overflow() {
        assert_eq!(parse_duration("999999999999999999h"), None);
    }

    #[test]
    fn test_parse_duration_bare_number() {
        assert_eq!(parse_duration("120"), Some(std::time::Duration::from_secs(120)));
    }

    #[test]
    fn test_parse_duration_empty() {
        assert_eq!(parse_duration(""), None);
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
        };
        assert_eq!(config.min_severity(), Severity::Minor);
    }

    // --- Helper for building a minimal Config in tests ---
    fn minimal_config(repo: &str) -> Config {
        Config {
            repo: repo.to_string(),
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
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: 7,
            review_prs: false,
            fix_external_ci: false,
            review_filter: "all".to_string(),
            review_fix_threshold: 7,
            concurrency: 3,
            investigate_issues: "".to_string(),
            max_issues: 2,
            issue_budget: 3.0,
            max_open_prs: 5,
            context_window: 128_000,
            max_pr_fix_attempts: 3,
            exa_searches: 3,
        }
    }

    #[test]
    fn test_repo_slug_ssh_url_without_git_suffix() {
        let config = minimal_config("git@github.com:owner/repo");
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_ssh_url_gitlab() {
        let config = minimal_config("git@gitlab.com:myorg/myrepo.git");
        assert_eq!(config.repo_slug(), "myorg/myrepo");
    }

    #[test]
    fn test_repo_slug_github_dot_com_prefix() {
        let config = minimal_config("github.com/owner/repo");
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_trailing_slash() {
        let config = minimal_config("owner/repo/");
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_http_url() {
        let config = minimal_config("http://github.com/owner/repo");
        assert_eq!(config.repo_slug(), "owner/repo");
    }

    #[test]
    fn test_repo_slug_ssh_url_with_nested_org() {
        // GitLab supports nested groups like org/subgroup/repo
        let config = minimal_config("git@gitlab.com:org/subgroup/repo.git");
        assert_eq!(config.repo_slug(), "org/subgroup/repo");
    }

    #[test]
    #[should_panic(expected = "path-traversal sequence")]
    fn test_repo_slug_rejects_path_traversal() {
        let config = minimal_config("https://github.com/../etc/passwd");
        config.repo_slug();
    }

    #[test]
    #[should_panic(expected = "path-traversal sequence")]
    fn test_repo_slug_rejects_path_traversal_ssh() {
        let config = minimal_config("git@github.com:../etc/passwd.git");
        config.repo_slug();
    }

    #[test]
    #[should_panic(expected = "path-traversal sequence")]
    fn test_repo_slug_rejects_path_traversal_mid_segment() {
        let config = minimal_config("https://github.com/owner/../other/repo");
        config.repo_slug();
    }

    #[test]
    fn test_validate_slug_accepts_valid() {
        // Should not panic
        validate_slug("owner/repo");
        validate_slug("my-org/my-repo");
        validate_slug("a/b/c");
    }

    #[test]
    #[should_panic(expected = "path-traversal sequence")]
    fn test_validate_slug_rejects_dotdot() {
        validate_slug("..");
    }

    #[test]
    #[should_panic(expected = "path-traversal sequence")]
    fn test_validate_slug_rejects_dotdot_in_path() {
        validate_slug("foo/../bar");
    }
}
