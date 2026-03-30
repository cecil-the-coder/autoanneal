use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct ManagerConfig {
    pub manager: ManagerSettings,
    #[serde(default)]
    pub defaults: WorkerDefaults,
    pub repos: Vec<RepoEntry>,
}

impl ManagerConfig {
    /// Validate configuration values. Call after loading config.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.manager.global_concurrency == 0 {
            anyhow::bail!("manager.global_concurrency must be > 0");
        }

        if self.manager.worker_image.is_empty() {
            anyhow::bail!("manager.worker_image must not be empty");
        }

        // Validate listen_address is parseable as a socket address
        self.manager.listen_addr.parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow::anyhow!(
                "manager.listen_addr '{}' is not a valid socket address: {}",
                self.manager.listen_addr, e
            ))?;

        for (i, repo) in self.repos.iter().enumerate() {
            if repo.name.is_empty() {
                anyhow::bail!("repos[{}].name must not be empty", i);
            }
            if !is_dns_safe(&repo.name) {
                anyhow::bail!(
                    "repos[{}].name '{}' is not DNS-safe (must be lowercase alphanumeric + hyphens, \
                     cannot start/end with hyphen)",
                    i, repo.name
                );
            }
            if repo.repo.is_empty() {
                anyhow::bail!("repos[{}].repo must not be empty", i);
            }
        }

        Ok(())
    }
}

/// Check that a name is DNS-safe: lowercase alphanumeric + hyphens, no leading/trailing hyphen.
fn is_dns_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManagerSettings {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_concurrency")]
    pub global_concurrency: usize,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default = "default_worker_image")]
    pub worker_image: String,
    #[serde(default = "default_result_path")]
    pub result_path: String,
    #[serde(default = "default_true")]
    pub docker_mode: bool,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub resource_cpu_limit: Option<String>,
    #[serde(default)]
    pub resource_memory_limit: Option<String>,
    /// Bearer token for API authentication. None = no auth (backward compat).
    #[serde(default)]
    pub api_token: Option<String>,
    /// Webhook cooldown in seconds (default 120).
    #[serde(default = "default_webhook_cooldown_secs")]
    pub webhook_cooldown_secs: u64,
    /// Poll interval in seconds for checking worker status (default 5).
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Maximum number of recent runs to keep in history (default 100).
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
}

fn default_listen_addr() -> String { "0.0.0.0:8080".into() }
fn default_concurrency() -> usize { 3 }
fn default_webhook_cooldown_secs() -> u64 { 120 }
fn default_poll_interval_secs() -> u64 { 5 }
fn default_history_limit() -> usize { 100 }
fn default_worker_image() -> String { "autoanneal:latest".into() }
fn default_result_path() -> String { "/tmp/autoanneal-result.json".into() }
fn default_true() -> bool { true }
fn default_namespace() -> String { "default".into() }

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WorkerDefaults {
    #[serde(default = "default_max_budget")]
    pub max_budget: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub model_recon: Option<String>,
    #[serde(default)]
    pub model_analysis: Option<String>,
    #[serde(default)]
    pub model_implement: Option<String>,
    #[serde(default)]
    pub model_critic: Option<String>,
    #[serde(default)]
    pub model_plan: Option<String>,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,
    #[serde(default = "default_min_severity")]
    pub min_severity: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default = "default_skip_after")]
    pub skip_after: usize,
    #[serde(default = "default_cron_interval")]
    pub cron_interval: u64,
    #[serde(default = "default_true")]
    pub fix_ci: bool,
    #[serde(default = "default_true")]
    pub fix_conflicts: bool,
    #[serde(default)]
    pub critic_threshold: u32,
    #[serde(default)]
    pub critic_models: Option<String>,
    #[serde(default = "default_true")]
    pub improve_docs: bool,
    #[serde(default = "default_doc_critic_threshold")]
    pub doc_critic_threshold: u32,
    #[serde(default)]
    pub fix_external_ci: bool,
    #[serde(default)]
    pub review_prs: bool,
    #[serde(default)]
    pub force_review: bool,
    #[serde(default = "default_review_filter")]
    pub review_filter: String,
    #[serde(default = "default_review_fix_threshold")]
    pub review_fix_threshold: u32,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_max_open_prs")]
    pub max_open_prs: usize,
    #[serde(default)]
    pub investigate_issues: String,
    #[serde(default = "default_max_issues")]
    pub max_issues: usize,
    #[serde(default = "default_issue_budget")]
    pub issue_budget: String,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
}

impl Default for WorkerDefaults {
    fn default() -> Self {
        Self {
            max_budget: default_max_budget(),
            timeout: default_timeout(),
            model: default_model(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: default_max_tasks(),
            min_severity: default_min_severity(),
            log_level: default_log_level(),
            dry_run: false,
            setup_command: None,
            skip_after: default_skip_after(),
            cron_interval: default_cron_interval(),
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: default_doc_critic_threshold(),
            fix_external_ci: false,
            review_prs: false,
            force_review: false,
            review_filter: default_review_filter(),
            review_fix_threshold: default_review_fix_threshold(),
            concurrency: default_concurrency(),
            max_open_prs: default_max_open_prs(),
            investigate_issues: String::new(),
            max_issues: default_max_issues(),
            issue_budget: default_issue_budget(),
            context_window: default_context_window(),
        }
    }
}

fn default_max_budget() -> String { "5.00".into() }
fn default_timeout() -> String { "30m".into() }
fn default_model() -> String { "sonnet".into() }
fn default_max_tasks() -> usize { 3 }
fn default_min_severity() -> String { "minor".into() }
fn default_log_level() -> String { "info".into() }
fn default_skip_after() -> usize { 3 }
fn default_cron_interval() -> u64 { 10 }
fn default_doc_critic_threshold() -> u32 { 7 }
fn default_review_filter() -> String { "all".into() }
fn default_review_fix_threshold() -> u32 { 7 }
fn default_max_open_prs() -> usize { 5 }
fn default_max_issues() -> usize { 2 }
fn default_issue_budget() -> String { "3.00".into() }
fn default_context_window() -> u64 { 128000 }

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RepoEntry {
    pub name: String,
    pub repo: String,
    #[serde(default)]
    pub schedule: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Per-repo overrides (all optional, fall back to defaults)
    #[serde(default)]
    pub max_budget: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_recon: Option<String>,
    #[serde(default)]
    pub model_analysis: Option<String>,
    #[serde(default)]
    pub model_implement: Option<String>,
    #[serde(default)]
    pub model_critic: Option<String>,
    #[serde(default)]
    pub model_plan: Option<String>,
    #[serde(default)]
    pub max_tasks: Option<usize>,
    #[serde(default)]
    pub min_severity: Option<String>,
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub skip_after: Option<usize>,
    #[serde(default)]
    pub cron_interval: Option<u64>,
    #[serde(default)]
    pub fix_ci: Option<bool>,
    #[serde(default)]
    pub fix_conflicts: Option<bool>,
    #[serde(default)]
    pub critic_threshold: Option<u32>,
    #[serde(default)]
    pub critic_models: Option<String>,
    #[serde(default)]
    pub improve_docs: Option<bool>,
    #[serde(default)]
    pub doc_critic_threshold: Option<u32>,
    #[serde(default)]
    pub fix_external_ci: Option<bool>,
    #[serde(default)]
    pub review_prs: Option<bool>,
    #[serde(default)]
    pub force_review: Option<bool>,
    #[serde(default)]
    pub review_filter: Option<String>,
    #[serde(default)]
    pub review_fix_threshold: Option<u32>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub max_open_prs: Option<usize>,
    #[serde(default)]
    pub investigate_issues: Option<String>,
    #[serde(default)]
    pub max_issues: Option<usize>,
    #[serde(default)]
    pub issue_budget: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

impl RepoEntry {
    /// Build the CLI arguments for the worker binary, resolving overrides against defaults.
    pub fn to_worker_args(&self, defaults: &WorkerDefaults) -> Vec<String> {
        let mut args = vec![
            self.repo.clone(),
            "--max-budget".to_string(), self.max_budget.as_ref().unwrap_or(&defaults.max_budget).clone(),
            "--timeout".to_string(), self.timeout.as_ref().unwrap_or(&defaults.timeout).clone(),
            "--model".to_string(), self.model.as_ref().unwrap_or(&defaults.model).clone(),
            "--max-tasks".to_string(), self.max_tasks.unwrap_or(defaults.max_tasks).to_string(),
            "--min-severity".to_string(), self.min_severity.as_ref().unwrap_or(&defaults.min_severity).clone(),
            "--log-level".to_string(), self.log_level.as_ref().unwrap_or(&defaults.log_level).clone(),
            "--skip-after".to_string(), self.skip_after.unwrap_or(defaults.skip_after).to_string(),
            "--cron-interval".to_string(), self.cron_interval.unwrap_or(defaults.cron_interval).to_string(),
            "--critic-threshold".to_string(), self.critic_threshold.unwrap_or(defaults.critic_threshold).to_string(),
            "--doc-critic-threshold".to_string(), self.doc_critic_threshold.unwrap_or(defaults.doc_critic_threshold).to_string(),
            "--review-filter".to_string(), self.review_filter.as_ref().unwrap_or(&defaults.review_filter).clone(),
            "--review-fix-threshold".to_string(), self.review_fix_threshold.unwrap_or(defaults.review_fix_threshold).to_string(),
            "--concurrency".to_string(), self.concurrency.unwrap_or(defaults.concurrency).to_string(),
            "--max-open-prs".to_string(), self.max_open_prs.unwrap_or(defaults.max_open_prs).to_string(),
            "--max-issues".to_string(), self.max_issues.unwrap_or(defaults.max_issues).to_string(),
            "--issue-budget".to_string(), self.issue_budget.as_ref().unwrap_or(&defaults.issue_budget).clone(),
            "--context-window".to_string(), self.context_window.unwrap_or(defaults.context_window).to_string(),
        ];

        // Boolean flags (clap uses ArgAction::Set, so all require explicit true/false value)
        let fix_ci = self.fix_ci.unwrap_or(defaults.fix_ci);
        args.extend_from_slice(&["--fix-ci".into(), fix_ci.to_string()]);
        let fix_conflicts = self.fix_conflicts.unwrap_or(defaults.fix_conflicts);
        args.extend_from_slice(&["--fix-conflicts".into(), fix_conflicts.to_string()]);
        let improve_docs = self.improve_docs.unwrap_or(defaults.improve_docs);
        args.extend_from_slice(&["--improve-docs".into(), improve_docs.to_string()]);
        let dry_run = self.dry_run.unwrap_or(defaults.dry_run);
        if dry_run { args.push("--dry-run".into()); }
        let fix_external_ci = self.fix_external_ci.unwrap_or(defaults.fix_external_ci);
        args.extend_from_slice(&["--fix-external-ci".into(), fix_external_ci.to_string()]);
        let review_prs = self.review_prs.unwrap_or(defaults.review_prs);
        args.extend_from_slice(&["--review-prs".into(), review_prs.to_string()]);
        let force_review = self.force_review.unwrap_or(defaults.force_review);
        if force_review { args.push("--force-review".into()); }

        // Investigate issues
        let investigate = self.investigate_issues.as_deref().unwrap_or(&defaults.investigate_issues);
        if !investigate.is_empty() {
            args.extend_from_slice(&["--investigate-issues".into(), investigate.to_string()]);
        }

        // Optional string flags
        if let Some(v) = self.setup_command.as_ref().or(defaults.setup_command.as_ref()) {
            args.extend_from_slice(&["--setup-command".into(), v.clone()]);
        }
        if let Some(v) = self.model_recon.as_ref().or(defaults.model_recon.as_ref()) {
            args.extend_from_slice(&["--model-recon".into(), v.clone()]);
        }
        if let Some(v) = self.model_analysis.as_ref().or(defaults.model_analysis.as_ref()) {
            args.extend_from_slice(&["--model-analysis".into(), v.clone()]);
        }
        if let Some(v) = self.model_implement.as_ref().or(defaults.model_implement.as_ref()) {
            args.extend_from_slice(&["--model-implement".into(), v.clone()]);
        }
        if let Some(v) = self.model_critic.as_ref().or(defaults.model_critic.as_ref()) {
            args.extend_from_slice(&["--model-critic".into(), v.clone()]);
        }
        if let Some(v) = self.model_plan.as_ref().or(defaults.model_plan.as_ref()) {
            args.extend_from_slice(&["--model-plan".into(), v.clone()]);
        }
        if let Some(v) = self.critic_models.as_ref().or(defaults.critic_models.as_ref()) {
            args.extend_from_slice(&["--critic-models".into(), v.clone()]);
        }

        args
    }
}

/// CLI args for the manager binary.
#[derive(Debug, clap::Parser)]
#[command(name = "autoanneal-manager", about = "Manager for autoanneal workers")]
pub struct ManagerCli {
    /// Path to config file (YAML)
    #[arg(long, default_value = "/config/manager.yaml")]
    pub config: PathBuf,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

pub fn load_config(path: &std::path::Path) -> anyhow::Result<ManagerConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: ManagerConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_repo_entry(name: &str, repo: &str) -> RepoEntry {
        RepoEntry {
            name: name.into(),
            repo: repo.into(),
            schedule: String::new(),
            enabled: true,
            max_budget: None,
            timeout: None,
            model: None,
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: None,
            min_severity: None,
            log_level: None,
            dry_run: None,
            setup_command: None,
            skip_after: None,
            cron_interval: None,
            fix_ci: None,
            fix_conflicts: None,
            critic_threshold: None,
            critic_models: None,
            improve_docs: None,
            doc_critic_threshold: None,
            fix_external_ci: None,
            review_prs: None,
            force_review: None,
            review_filter: None,
            review_fix_threshold: None,
            concurrency: None,
            max_open_prs: None,
            investigate_issues: None,
            max_issues: None,
            issue_budget: None,
            context_window: None,
        }
    }

    #[test]
    fn test_default_config_loads() {
        let defaults = WorkerDefaults::default();
        assert_eq!(defaults.max_budget, "5.00");
        assert_eq!(defaults.timeout, "30m");
        assert_eq!(defaults.model, "sonnet");
        assert_eq!(defaults.max_tasks, 3);
        assert_eq!(defaults.min_severity, "minor");
        assert_eq!(defaults.log_level, "info");
        assert!(!defaults.dry_run);
        assert!(defaults.fix_ci);
        assert!(defaults.fix_conflicts);
        assert!(defaults.improve_docs);
        assert!(!defaults.fix_external_ci);
        assert!(!defaults.review_prs);
        assert_eq!(defaults.skip_after, 3);
        assert_eq!(defaults.cron_interval, 10);
        assert_eq!(defaults.concurrency, 3);
        assert_eq!(defaults.max_open_prs, 5);
        assert_eq!(defaults.context_window, 128000);
    }

    #[test]
    fn test_repo_to_worker_args() {
        let defaults = WorkerDefaults::default();
        let entry = minimal_repo_entry("test-repo", "owner/repo");
        let args = entry.to_worker_args(&defaults);

        // First arg is the repo slug
        assert_eq!(args[0], "owner/repo");

        let check = |flag: &str, val: &str| {
            let idx = args.iter().position(|a| a == flag)
                .unwrap_or_else(|| panic!("missing flag {flag}"));
            assert_eq!(args[idx + 1], val, "wrong value for {flag}");
        };

        check("--max-budget", "5.00");
        check("--model", "sonnet");
        // Default boolean flags (all use --flag value format)
        check("--fix-ci", "true");
        check("--fix-conflicts", "true");
        check("--improve-docs", "true");
        assert!(!args.contains(&"--dry-run".to_string()));
        check("--fix-external-ci", "false");
        check("--review-prs", "false");
    }

    #[test]
    fn test_repo_to_worker_args_with_overrides() {
        let defaults = WorkerDefaults::default();
        let mut entry = minimal_repo_entry("test-repo", "owner/repo");
        entry.max_budget = Some("10.00".into());
        entry.model = Some("opus".into());
        entry.dry_run = Some(true);
        entry.fix_ci = Some(false);

        let args = entry.to_worker_args(&defaults);

        // Overridden values
        let budget_idx = args.iter().position(|a| a == "--max-budget").unwrap();
        assert_eq!(args[budget_idx + 1], "10.00");

        let model_idx = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[model_idx + 1], "opus");

        let check = |flag: &str, val: &str| {
            let idx = args.iter().position(|a| a == flag)
                .unwrap_or_else(|| panic!("missing flag {flag}"));
            assert_eq!(args[idx + 1], val, "wrong value for {flag}");
        };
        assert!(args.contains(&"--dry-run".to_string()));
        check("--fix-ci", "false");
    }

    #[test]
    fn test_repo_to_worker_args_all_fields() {
        let defaults = WorkerDefaults::default();
        let mut entry = minimal_repo_entry("test-repo", "owner/repo");
        entry.max_budget = Some("15.00".into());
        entry.timeout = Some("60m".into());
        entry.model = Some("haiku".into());
        entry.model_recon = Some("sonnet".into());
        entry.model_analysis = Some("opus".into());
        entry.model_implement = Some("sonnet".into());
        entry.model_critic = Some("opus".into());
        entry.model_plan = Some("haiku".into());
        entry.max_tasks = Some(10);
        entry.min_severity = Some("major".into());
        entry.log_level = Some("debug".into());
        entry.dry_run = Some(true);
        entry.setup_command = Some("make setup".into());
        entry.skip_after = Some(5);
        entry.cron_interval = Some(30);
        entry.fix_ci = Some(false);
        entry.fix_conflicts = Some(false);
        entry.critic_threshold = Some(8);
        entry.critic_models = Some("opus,sonnet".into());
        entry.improve_docs = Some(false);
        entry.doc_critic_threshold = Some(9);
        entry.fix_external_ci = Some(true);
        entry.review_prs = Some(true);
        entry.review_filter = Some("labeled".into());
        entry.review_fix_threshold = Some(5);
        entry.concurrency = Some(2);
        entry.max_open_prs = Some(10);
        entry.investigate_issues = Some("all".into());
        entry.max_issues = Some(5);
        entry.issue_budget = Some("8.00".into());
        entry.context_window = Some(200000);

        let args = entry.to_worker_args(&defaults);

        // Verify all value flags
        let check = |flag: &str, val: &str| {
            let idx = args.iter().position(|a| a == flag)
                .unwrap_or_else(|| panic!("missing flag {flag}"));
            assert_eq!(args[idx + 1], val, "wrong value for {flag}");
        };

        check("--max-budget", "15.00");
        check("--timeout", "60m");
        check("--model", "haiku");
        check("--model-recon", "sonnet");
        check("--model-analysis", "opus");
        check("--model-implement", "sonnet");
        check("--model-critic", "opus");
        check("--model-plan", "haiku");
        check("--max-tasks", "10");
        check("--min-severity", "major");
        check("--log-level", "debug");
        check("--skip-after", "5");
        check("--cron-interval", "30");
        check("--critic-threshold", "8");
        check("--critic-models", "opus,sonnet");
        check("--doc-critic-threshold", "9");
        check("--review-filter", "labeled");
        check("--review-fix-threshold", "5");
        check("--concurrency", "2");
        check("--max-open-prs", "10");
        check("--max-issues", "5");
        check("--issue-budget", "8.00");
        check("--context-window", "200000");
        check("--setup-command", "make setup");
        check("--investigate-issues", "all");

        // Boolean flags (all use --flag value format)
        check("--fix-ci", "false");
        check("--fix-conflicts", "false");
        check("--improve-docs", "false");
        assert!(args.contains(&"--dry-run".to_string()));
        check("--fix-external-ci", "true");
        check("--review-prs", "true");
    }

    #[test]
    fn test_config_from_yaml() {
        let yaml = r#"
manager:
  listen_addr: "0.0.0.0:9090"
  global_concurrency: 5
  webhook_secret: "mysecret"
  worker_image: "myimage:v1"

defaults:
  max_budget: "10.00"
  model: "opus"

repos:
  - name: my-repo
    repo: owner/my-repo
    schedule: "*/5 * * * *"
  - name: other-repo
    repo: owner/other-repo
    enabled: false
"#;
        let config: ManagerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.manager.listen_addr, "0.0.0.0:9090");
        assert_eq!(config.manager.global_concurrency, 5);
        assert_eq!(config.manager.webhook_secret, "mysecret");
        assert_eq!(config.manager.worker_image, "myimage:v1");
        assert_eq!(config.defaults.max_budget, "10.00");
        assert_eq!(config.defaults.model, "opus");
        assert_eq!(config.repos.len(), 2);
        assert_eq!(config.repos[0].name, "my-repo");
        assert_eq!(config.repos[0].repo, "owner/my-repo");
        assert!(config.repos[0].enabled);
        assert!(!config.repos[1].enabled);
    }

    #[test]
    fn test_config_defaults_applied() {
        let yaml = r#"
manager: {}
repos:
  - name: minimal
    repo: owner/minimal
"#;
        let config: ManagerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.manager.listen_addr, "0.0.0.0:8080");
        assert_eq!(config.manager.global_concurrency, 3);
        assert_eq!(config.manager.worker_image, "autoanneal:latest");
        assert_eq!(config.manager.result_path, "/tmp/autoanneal-result.json");
        assert!(config.manager.docker_mode);
        assert_eq!(config.manager.webhook_secret, "");
        // Defaults block should get WorkerDefaults::default()
        assert_eq!(config.defaults.max_budget, "5.00");
        assert_eq!(config.defaults.timeout, "30m");
        assert_eq!(config.defaults.model, "sonnet");
        // Repo should default to enabled
        assert!(config.repos[0].enabled);
    }
}
