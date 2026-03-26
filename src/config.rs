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

    /// Claude model alias or ID
    #[arg(long, default_value = "sonnet")]
    pub model: String,

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
}

impl Config {
    /// Parse the repo string into "owner/repo" format.
    /// Handles both "owner/repo" and "https://github.com/owner/repo" formats.
    pub fn repo_slug(&self) -> String {
        let s = &self.repo;

        let slug = if let Some(rest) = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("http://github.com/"))
            .or_else(|| s.strip_prefix("github.com/"))
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
    pub fn timeout_duration(&self) -> std::time::Duration {
        parse_duration(&self.timeout).unwrap_or(std::time::Duration::from_secs(30 * 60))
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

            match c {
                'h' | 'H' => total_secs += n * 3600,
                'm' | 'M' => total_secs += n * 60,
                's' | 'S' => total_secs += n,
                _ => return None,
            }
        }
    }

    // Handle bare number (no suffix) — treat as seconds
    if !current_num.is_empty() {
        let n: u64 = current_num.parse().ok()?;
        total_secs += n;
    }

    if total_secs == 0 {
        return None;
    }

    Some(std::time::Duration::from_secs(total_secs))
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
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
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
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
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
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "minor".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
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
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "moderate".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
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
            max_tasks: 5,
            dry_run: false,
            keep_on_failure: false,
            setup_command: None,
            min_severity: "unknown".to_string(),
            log_level: "info".to_string(),
            output: "text".to_string(),
        };
        assert_eq!(config.min_severity(), Severity::Minor);
    }
}
